//! Native platform: asupersync HTTP, SSE and WebSockets, the system
//! clipboard, and half-block rendering of remote screens.

use crate::app::ScreenFrame;
pub use crate::client::{Remote, SocketEvent, StreamUpdate, Update};
use anyhow::{Context, Result, bail};
pub use asupersync::Cx;
use asupersync::channel::mpsc;
pub use asupersync::http::h1::Method;
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use asupersync::net::TcpStream;
use asupersync::net::websocket::{
    ClientHandshake, HttpResponse, Message, WebSocket, WebSocketConfig,
};
use asupersync::tls::{Certificate, TlsConnector};
use ftui::Frame;
use ftui::core::geometry::Rect;
use ftui::style::{Color, Rgb, Style};
use ftui::text::{Line, Span};
use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub const CAN_QUIT: bool = true;
pub fn dev() -> bool {
    false
}

pub type Tx = mpsc::Sender<Update>;
pub type Rx = mpsc::Receiver<Update>;

pub fn channel() -> (Tx, Rx) {
    mpsc::channel(256)
}

/// Set by `/logout` so the launcher forgets the saved session.
pub static LOGGED_OUT: AtomicBool = AtomicBool::new(false);

pub struct Feed(asupersync::runtime::TaskHandle<()>);

impl Feed {
    pub fn stop(self) {
        self.0.abort();
    }
}

pub fn follow(cx: &Cx, remote: &Remote, id: String, cursor: i64, en: bool, tx: Tx) -> Option<Feed> {
    let remote = remote.clone();
    cx.spawn(move |child| async move { remote.follow(child, id, cursor, en, tx).await })
        .ok()
        .map(Feed)
}

pub fn now_ms() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

pub fn bell() {
    let _ = write!(io::stdout(), "\x07");
    let _ = io::stdout().flush();
}

pub fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_text())
        .ok()
}

pub fn copy(text: &str) {
    if let Ok(mut clip) = arboard::Clipboard::new()
        && clip.set_text(text.to_owned()).is_ok()
    {
        return;
    }
    // OSC 52 fallback for remote/SSH terminals.
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::with_capacity(text.len().div_ceil(3) * 4);
    for chunk in text.as_bytes().chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            b64.push(if i <= chunk.len() {
                TABLE[((n >> shift) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    let _ = write!(io::stdout(), "\x1b]52;c;{b64}\x07");
    let _ = io::stdout().flush();
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn set_location(_path: &str) {}

pub fn save_language(_en: bool) {}

pub fn open_url(url: &str) {
    let _ = open::that_detached(url);
}

pub fn navigate(url: &str) {
    open_url(url);
}

pub fn signed_out(_redirect: &str, _portal: &str) {
    LOGGED_OUT.store(true, Ordering::SeqCst);
}

pub fn sign_in(_dev: bool) {}

/// The console signs in before the UI starts.
pub async fn token_login(_cx: &Cx) -> bool {
    false
}

pub async fn login(_cx: &Cx, _token: &str) -> Result<bool> {
    bail!("Restart venticode-tui to sign in")
}

#[cfg(test)]
pub fn test_remote() -> Remote {
    let saved =
        serde_json::json!({"url": "https://workspace.example", "cookie": "venticode_session=test"});
    Remote::restore(None, &saved.to_string(), false).expect("test remote")
}

// ----- WebSockets ----------------------------------------------------------

pub struct Socket {
    out: mpsc::Sender<String>,
    task: asupersync::runtime::TaskHandle<()>,
}

impl Socket {
    pub fn send(&self, text: String) -> bool {
        self.out.try_send(text).is_ok()
    }
    pub fn close(self) {
        self.task.abort();
    }
}

/// Open `path` (under the API prefix) as a WebSocket carrying the session
/// cookie and the workspace Origin, and pump it into `tx`.
pub fn socket(cx: &Cx, remote: &Remote, path: &str, id: u64, tx: Tx) -> Result<Socket> {
    let (out, rx) = mpsc::channel::<String>(64);
    let remote = remote.clone();
    let path = format!("/api/v1/venticode{path}");
    let task = cx
        .spawn(move |cx| async move {
            let mut rx = rx;
            let _ = run_socket(&cx, &remote, &path, id, &tx, &mut rx).await;
            let _ = tx.send(&cx, Update::Socket(id, SocketEvent::Closed)).await;
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Socket { out, task })
}

async fn run_socket(
    cx: &Cx,
    remote: &Remote,
    path: &str,
    id: u64,
    tx: &Tx,
    out: &mut mpsc::Receiver<String>,
) -> Result<()> {
    let url = url::Url::parse(&remote.workspace)?;
    let host = url.host_str().context("host")?.to_owned();
    let port = url.port_or_known_default().context("port")?;
    let tls = url.scheme() == "https";
    let target = format!(
        "{}://{}{path}",
        if tls { "wss" } else { "ws" },
        url.authority()
    );
    let tcp = TcpStream::connect((host.clone(), port)).await?;
    if tls {
        let mut builder = TlsConnector::builder()
            .with_webpki_roots()
            .alpn_protocols(vec![b"http/1.1".to_vec()]);
        if let Some(ca) = &remote.ca {
            for cert in Certificate::from_pem_file(ca).map_err(|e| anyhow::anyhow!("{e}"))? {
                builder = builder.add_root_certificate(&cert);
            }
        }
        let connector = builder.build().map_err(|e| anyhow::anyhow!("{e}"))?;
        let stream = connector
            .connect(&host, tcp)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        pump(cx, stream, &target, remote, id, tx, out).await
    } else {
        pump(cx, tcp, &target, remote, id, tx, out).await
    }
}

async fn pump<IO>(
    cx: &Cx,
    mut io: IO,
    target: &str,
    remote: &Remote,
    id: u64,
    tx: &Tx,
    out: &mut mpsc::Receiver<String>,
) -> Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let handshake = ClientHandshake::new(target, cx.entropy())
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .header("Cookie", remote.cookie.clone())
        .header("Origin", remote.workspace.clone());
    io.write_all(&handshake.request_bytes()).await?;
    io.flush().await?;
    // Read the upgrade response byte by byte so no frame bytes are lost.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if io.read(&mut byte).await? == 0 || head.len() > 16 * 1024 {
            bail!("WebSocket upgrade failed");
        }
        head.push(byte[0]);
    }
    let response = HttpResponse::parse(&head).map_err(|e| anyhow::anyhow!("{e}"))?;
    handshake
        .validate_response(&response)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let ws = WebSocket::from_upgraded(io, WebSocketConfig::default());
    let (mut read, mut write) = ws.split();
    loop {
        // Flush queued outgoing packets before waiting for the next frame.
        while let Ok(text) = out.try_recv() {
            write
                .send(cx, Message::text(text))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let next = asupersync::time::timeout(
            asupersync::time::wall_now(),
            std::time::Duration::from_millis(30),
            read.recv(cx),
        )
        .await;
        let event = match next {
            Err(_) => continue,
            Ok(Ok(Some(Message::Text(text)))) => SocketEvent::Text(text),
            Ok(Ok(Some(Message::Binary(bytes)))) => SocketEvent::Binary(bytes.to_vec()),
            Ok(Ok(Some(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Ok(Some(Message::Close(_)) | None)) => return Ok(()),
            Ok(Err(e)) => bail!("{e}"),
        };
        if tx.send(cx, Update::Socket(id, event)).await.is_err() {
            return Ok(());
        }
    }
}

// ----- Remote screens --------------------------------------------------------

type Cells = (u64, u16, u16, Vec<(Rgb, Rgb)>);

thread_local! {
    static IMAGE: RefCell<Option<Cells>> = const { RefCell::new(None) };
}

fn decode(jpeg: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let options = zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
        zune_core::bytestream::ZCursor::new(jpeg),
        options,
    );
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    Some((usize::from(info.width), usize::from(info.height), pixels))
}

/// Draw a frame as half blocks: each cell shows two vertical pixels.
pub fn draw_image(f: &mut Frame, area: Rect, frame: &ScreenFrame) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let cells = IMAGE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(c) = cache.as_ref()
            && c.0 == frame.serial
            && c.1 == area.width
            && c.2 == area.height
        {
            return c.3.clone();
        }
        let Some((w, h, rgb)) = decode(&frame.jpeg) else {
            return Vec::new();
        };
        let (cols, rows) = (usize::from(area.width), usize::from(area.height));
        let pixel = |x: usize, y: usize| {
            let i = (y.min(h - 1) * w + x.min(w - 1)) * 3;
            Rgb::new(rgb[i], rgb[i + 1], rgb[i + 2])
        };
        let mut out = Vec::with_capacity(cols * rows);
        for cy in 0..rows {
            for cx in 0..cols {
                let x = (cx * 2 + 1) * w / (cols * 2);
                let top = (cy * 4 + 1) * h / (rows * 4);
                let bottom = (cy * 4 + 3) * h / (rows * 4);
                out.push((pixel(x, top), pixel(x, bottom)));
            }
        }
        *cache = Some((frame.serial, area.width, area.height, out.clone()));
        out
    });
    if cells.is_empty() {
        return;
    }
    let lines = cells.chunks(usize::from(area.width)).map(|row| {
        Line::from_spans(
            row.iter()
                .map(|(top, bottom)| {
                    Span::styled(
                        "▀",
                        Style::new().fg(Color::Rgb(*top)).bg(Color::Rgb(*bottom)),
                    )
                })
                .collect::<Vec<_>>(),
        )
    });
    crate::view::paint(f, area, lines);
}
