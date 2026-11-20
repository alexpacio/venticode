#![recursion_limit = "512"]

mod app;
mod client;
mod editor;
mod login;
mod nav;
mod platform;
mod screens;
mod store;
#[path = "../../update/update.rs"]
mod update;
mod view;

use anyhow::{Context, Result, anyhow};
use app::App;
use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{sleep, wall_now};
use client::{Remote, origin};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use rich_rust::prelude::*;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const MAX_EVENTS_PER_FRAME: usize = 32;

fn restore_terminal(enhanced: bool) {
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    if enhanced {
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::style::ResetColor,
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Console driver: crossterm input on a thread, the shared core, and ftui
/// presenting the frame.
async fn run_ui(
    cx: &Cx,
    remote: Remote,
    store: store::SessionStore,
    en: bool,
    start: Option<String>,
) -> Result<()> {
    let (tx, mut rx) = platform::channel();
    let mut app = App::new(remote, en, tx);
    let path = start.map(|id| format!("/c/{id}")).unwrap_or_default();
    app.start(cx, &path).await?;

    crossterm::terminal::enable_raw_mode()?;
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            restore_terminal(self.0);
        }
    }
    let mut _restore = Restore(false);
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )?;
    }
    _restore.0 = enhanced;
    // Panics restore the terminal so the trace stays readable.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(enhanced);
        prev_hook(info);
    }));
    let terminated = Arc::new(AtomicBool::new(false));
    let term_flag = terminated.clone();
    let _ = ctrlc::set_handler(move || term_flag.store(true, Ordering::SeqCst));

    // A dedicated input thread keeps the async runtime responsive.
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader_flag = stop_reader.clone();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<io::Result<crossterm::event::Event>>();
    let reader_thread = std::thread::spawn(move || {
        while !reader_flag.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(40)) {
                Ok(true) => {
                    if ev_tx.send(crossterm::event::read()).is_err() {
                        break;
                    }
                }
                Ok(false) => {}
                Err(err) => {
                    let _ = ev_tx.send(Err(err));
                    break;
                }
            }
        }
    });

    let mut terminal = ftui::TerminalWriter::new(
        io::stdout(),
        ftui::ScreenMode::AltScreen,
        ftui::UiAnchor::Bottom,
        ftui::TerminalCapabilities::detect(),
    );
    let mut last_size = (0, 0);
    let mut expired = false;
    'ui: loop {
        if terminated.load(Ordering::Relaxed) || app.quit {
            break;
        }
        let mut busy = false;
        while let Ok(update) = rx.try_recv() {
            app.update(update);
            busy = true;
        }
        for _ in 0..MAX_EVENTS_PER_FRAME {
            let event = match ev_rx.try_recv() {
                Ok(item) => item?,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break 'ui,
            };
            busy = true;
            if let Some(event) = ftui::Event::from_crossterm(event) {
                app.handle(cx, event).await;
            }
            if app.quit {
                break 'ui;
            }
        }
        app.tick(cx).await;
        if app.screen == app::Screen::SignedOut && !expired {
            expired = true;
            store.clear();
        }
        let size = crossterm::terminal::size()?;
        if app.dirty || size != last_size {
            last_size = size;
            if (terminal.width(), terminal.height()) != size {
                terminal.set_size(size.0, size.1);
            }
            let buffer = terminal.take_render_buffer(size.0, size.1);
            let mut screen = ftui::Frame::from_buffer(buffer, terminal.pool_mut());
            app.draw(&mut screen);
            let (buffer, cursor) = (screen.buffer, screen.cursor_position);
            terminal.present_ui_owned(buffer, cursor, cursor.is_some())?;
            if terminal.pool().len() > usize::from(size.0) * usize::from(size.1) {
                terminal.gc(None);
            }
        }
        if !busy {
            sleep(wall_now(), Duration::from_millis(25)).await;
        }
    }
    stop_reader.store(true, Ordering::Relaxed);
    app.close();
    if platform::LOGGED_OUT.load(Ordering::SeqCst) {
        store.clear();
    }
    let _ = reader_thread.join();
    Ok(())
}

fn print_help() {
    let console = Console::new();
    let mut table = Table::new()
        .title(format!("venticode-tui v{}", env!("CARGO_PKG_VERSION")))
        .with_column(Column::new("Option").style(Style::parse("bold cyan").unwrap_or_default()))
        .with_column(Column::new("Description"));
    for (flag, desc) in [
        ("--portal <URL>", "Ventic portal HTTPS origin"),
        (
            "--standalone <URL>",
            "Standalone Venticode origin (uses access token)",
        ),
        (
            "--conversation <UUID>",
            "Open a specific conversation at startup",
        ),
        ("--en", "Use English UI copy"),
        ("--manual", "Paste the one-time sign-in code manually"),
        (
            "--no-browser",
            "Do not launch a system browser (implies --manual)",
        ),
        ("--ticket-stdin", "Read the one-time code from stdin"),
        ("--ca <PEM>", "Additional PEM root CA certificate"),
        ("--no-update", "Do not check for a newer release"),
        ("--version, -V", "Print version information"),
        ("--help, -h", "Show this help"),
    ] {
        table.add_row_cells([flag, desc]);
    }
    console.print_renderable(&table);
}

fn next_flag_value(
    flag: &str,
    inline: Option<&str>,
    args: &mut impl Iterator<Item = String>,
) -> Result<String> {
    if let Some(val) = inline {
        Ok(val.to_owned())
    } else {
        args.next()
            .with_context(|| format!("{flag} requires a value"))
    }
}

async fn fetch(cx: &Cx, url: String, timeout: Duration) -> Result<Vec<u8>> {
    let res = asupersync::http::h1::HttpClient::builder()
        .no_cookie_store()
        .build()
        .request_with_timeout(
            cx,
            asupersync::http::h1::Method::Get,
            &url,
            Vec::new(),
            Vec::new(),
            timeout,
        )
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if res.status != 200 {
        return Err(anyhow!("{url}: HTTP {}", res.status));
    }
    Ok(res.body)
}

/// Install a newer release over this binary and relaunch it; failures never block the app.
async fn self_update(cx: &Cx, console: &Console, en: bool) {
    let current = env!("CARGO_PKG_VERSION");
    match update::check("venticode-tui", current, |url, t| fetch(cx, url, t)).await {
        Ok(Some(done)) => {
            console.print(&format!(
                "[green]{}[/]",
                if en {
                    format!(
                        "Updated venticode-tui {current} -> {}, restarting.",
                        done.version
                    )
                } else {
                    format!(
                        "Aggiornato venticode-tui {current} -> {}, riavvio.",
                        done.version
                    )
                }
            ));
            let err = update::restart(&done);
            console.print(&format!("[yellow]Restart failed: {err}[/]"));
        }
        Ok(None) => {}
        Err(err) => console.print(&format!(
            "[dim]{}[/]",
            if en {
                format!("Update check skipped: {err:#}")
            } else {
                format!("Controllo aggiornamenti non riuscito: {err:#}")
            }
        )),
    }
}

async fn run_cli(cx: Cx) -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut portal = None;
    let mut standalone = None;
    let mut start = None;
    let mut stdin = false;
    let mut browser = true;
    let mut manual = false;
    let mut ca = None;
    let mut en = false;
    let mut updates = true;
    while let Some(arg) = args.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(f, v)| (f, Some(v)));
        match flag {
            "--portal" => portal = Some(next_flag_value("--portal", inline, &mut args)?),
            "--standalone" => {
                let raw = next_flag_value("--standalone", inline, &mut args)?;
                standalone = Some(client::standalone_origin(&raw)?);
            }
            "--conversation" => {
                start = Some(next_flag_value("--conversation", inline, &mut args)?);
            }
            "--ticket-stdin" => stdin = true,
            "--no-browser" => {
                browser = false;
                manual = true;
            }
            "--manual" => manual = true,
            "--ca" => {
                ca = Some(std::path::PathBuf::from(next_flag_value(
                    "--ca", inline, &mut args,
                )?));
            }
            "--en" => en = true,
            "--no-update" => updates = false,
            "--version" | "-V" => {
                println!("venticode-tui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => return Err(anyhow!("Unknown option: {arg}")),
        }
    }
    if standalone.is_some() && (portal.is_some() || stdin || manual) {
        return Err(anyhow!(
            "--standalone uses an access token and cannot be combined with the portal sign-in options"
        ));
    }
    let console = Console::new();
    if updates {
        self_update(&cx, &console, en).await;
    }
    let portal = match portal.or_else(|| standalone.clone()) {
        Some(value) if standalone.is_some() => value,
        Some(value) => origin(&value)?,
        None => {
            console.print(if en {
                "[bold cyan]Ventic portal HTTPS URL:[/]"
            } else {
                "[bold cyan]URL HTTPS del portale Ventic:[/]"
            });
            let mut value = String::new();
            io::stdin().read_line(&mut value)?;
            origin(value.trim())?
        }
    };
    let store = store::SessionStore::open(
        if standalone.is_some() {
            "venticode-tui-standalone"
        } else {
            "venticode-tui"
        },
        &portal,
    );
    let mut session = None;
    if !stdin && let Some(saved) = store.load() {
        match Remote::restore(ca.as_deref(), &saved, standalone.is_some()) {
            Ok(remote) => {
                match remote.valid(&cx).await {
                    Ok(true) => session = Some(remote),
                    Ok(false) => {
                        store.clear();
                    }
                    Err(err) => {
                        console.print(&format!(
                            "[yellow]{}[/]",
                            if en {
                                format!("Could not validate saved session ({err}); signing in again.")
                            } else {
                                format!("Impossibile verificare la sessione salvata ({err}); nuovo accesso.")
                            }
                        ));
                    }
                }
            }
            Err(err) => {
                console.print(&format!(
                    "[yellow]{}[/]",
                    if en {
                        format!("Ignoring corrupt saved session ({err}); signing in again.")
                    } else {
                        format!("Sessione salvata non valida ({err}); nuovo accesso.")
                    }
                ));
                store.clear();
            }
        }
    }
    let remote = if let Some(remote) = session {
        remote
    } else {
        let code = if standalone.is_some() {
            rpassword::prompt_password(if en {
                "Venticode access token: "
            } else {
                "Token di accesso Venticode: "
            })?
        } else if stdin {
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            line
        } else if manual {
            let url = format!("{portal}/app/venticode?terminal=1");
            let msg = if en {
                format!("Sign in in your browser:\n{url}")
            } else {
                format!("Accedi nel browser:\n{url}")
            };
            let panel = Panel::from_text(&msg)
                .title("Venticode")
                .border_style(Style::parse("cyan").unwrap_or_default());
            console.print_renderable(&panel);
            if browser {
                let _ = open::that_detached(&url);
            }
            rpassword::prompt_password(if en {
                "One-time code: "
            } else {
                "Codice monouso: "
            })?
        } else {
            login::browser(&cx, &portal, en).await?
        };
        let remote = if standalone.is_some() {
            Remote::connect_standalone(&cx, ca.as_deref(), &portal, code.trim()).await?
        } else {
            Remote::connect(&cx, ca.as_deref(), &portal, code.trim()).await?
        };
        if let Err(e) = store.save(&remote.saved()?) {
            console.print(&format!(
                "[dim]{}[/]",
                if en {
                    format!("Session available for this launch only: {e}")
                } else {
                    format!("Sessione solo in memoria: {e}")
                }
            ));
        } else if store.is_file_backend() {
            let where_ = store
                .file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            console.print(&format!(
                "[dim]{}[/]",
                if en {
                    format!("Session saved unencrypted in {where_}")
                } else {
                    format!("Sessione salvata senza cifratura in {where_}")
                }
            ));
        }
        remote
    };
    run_ui(&cx, remote, store, en, start).await
}

fn main() {
    let runtime = match RuntimeBuilder::new().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Failed to start async runtime: {err}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(async {
        let cx = Cx::current().expect("asupersync runtime context");
        run_cli(cx).await
    });
    if let Err(err) = result {
        let console = Console::new();
        let text = format!("{err:#}");
        let panel = Panel::from_text(&text)
            .title("venticode-tui")
            .border_style(Style::parse("bold red").unwrap_or_default());
        console.print_renderable(&panel);
        std::process::exit(1);
    }
}
