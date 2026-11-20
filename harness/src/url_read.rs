//! `fetch_url`: reader-mode URL reads, a minimal port of pi_agent_rust's url_read
//! (HTML → markdown via htmd, GitHub blob → raw rewrite, default-on SSRF guard).

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use url::Url;

const MAX_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 5;
const DEADLINE: Duration = Duration::from_secs(30);

pub async fn fetch(url: &str, raw: bool) -> Result<Value> {
    tokio::time::timeout(DEADLINE, fetch_inner(url, raw))
        .await
        .context("URL read timed out after 30s")?
}

async fn fetch_inner(input: &str, raw: bool) -> Result<Value> {
    let mut url = http_url(input)?;
    if let Some(rewritten) = github_blob_to_raw(&url) {
        url = rewritten;
    }
    for _ in 0..=MAX_REDIRECTS {
        let host = url.host_str().context("URL has no host")?.to_owned();
        let port = url.port_or_known_default().context("URL has no port")?;
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .with_context(|| format!("Cannot resolve {host}"))?
            .collect();
        ensure!(!addrs.is_empty(), "Cannot resolve {host}");
        ensure!(
            !addrs.iter().any(|a| private_ip(a.ip())),
            "Refusing a private, loopback or link-local target ({host})"
        );
        // Pin the checked addresses so a second DNS answer cannot rebind to a private host.
        let client = reqwest_mcp::Client::builder()
            .no_proxy()
            .redirect(reqwest_mcp::redirect::Policy::none())
            .resolve_to_addrs(&host, &addrs)
            .build()?;
        let mut response = client.get(url.clone()).send().await?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest_mcp::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("Redirect without a Location header")?;
            ensure!(
                location.len() <= 8192 && !location.contains('\\'),
                "Invalid Location header"
            );
            url = http_url(url.join(location.trim())?.as_str())?;
            continue;
        }
        let content_type = response
            .headers()
            .get(reqwest_mcp::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_ascii_lowercase();
        let html = content_type.contains("html");
        ensure!(
            html || content_type.starts_with("text/")
                || ["json", "xml", "javascript"]
                    .iter()
                    .any(|kind| content_type.contains(kind)),
            "Unsupported content type '{content_type}'"
        );
        let mut bytes = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response.chunk().await? {
            let room = MAX_DOWNLOAD_BYTES - bytes.len();
            if chunk.len() > room {
                bytes.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut content = if html && !raw {
            htmd::HtmlToMarkdown::builder()
                .skip_tags(vec![
                    "script", "style", "noscript", "nav", "footer", "header", "aside", "form",
                    "iframe", "svg",
                ])
                .build()
                .convert(&text)?
        } else {
            text.into_owned()
        };
        truncated |= truncate(&mut content, MAX_CONTENT_BYTES);
        return Ok(json!({
            "url": url.as_str(),
            "status": status.as_u16(),
            "content_type": content_type,
            "content": content,
            "truncated": truncated,
        }));
    }
    bail!("Too many redirects")
}

/// Cut `s` to at most `max` bytes on a char boundary; true when something was cut.
pub fn truncate(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    s.truncate(s.floor_char_boundary(max));
    true
}

fn http_url(input: &str) -> Result<Url> {
    let url = Url::parse(input.trim()).context("Invalid URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Only http and https URLs are supported"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URLs with credentials are not supported"
    );
    Ok(url)
}

/// github.com/<owner>/<repo>/blob/<ref>/<path> → raw.githubusercontent.com (ported from pi).
fn github_blob_to_raw(url: &Url) -> Option<Url> {
    if url.host_str() != Some("github.com") || url.port().is_some() {
        return None;
    }
    let parts: Vec<_> = url.path_segments()?.collect();
    if parts.len() < 5 || parts[2] != "blob" || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    if parts[..2].iter().any(|c| {
        matches!(*c, "." | "..")
            || !c
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    }) {
        return None;
    }
    Url::parse(&format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        parts[0],
        parts[1],
        parts[3],
        parts[4..].join("/")
    ))
    .ok()
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b)) // CGNAT
                || (a == 192 && b == 0 && v4.octets()[2] == 0)
                || (a == 198 && (b == 18 || b == 19)) // benchmarking
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return private_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x64 && s[1] == 0xff9b) // NAT64 can reach any IPv4
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_targets_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!private_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn github_blob_urls_map_to_raw() {
        let raw = |s: &str| github_blob_to_raw(&Url::parse(s).unwrap()).map(|u| u.to_string());
        assert_eq!(
            raw("https://github.com/o/r/blob/main/src/lib.rs?plain=1#L3").as_deref(),
            Some("https://raw.githubusercontent.com/o/r/main/src/lib.rs")
        );
        assert_eq!(raw("https://github.com/o/r/tree/main/src"), None);
        assert_eq!(raw("https://github.com:8443/o/r/blob/main/x"), None);
        assert_eq!(raw("https://github.com.evil/o/r/blob/main/x"), None);
        assert_eq!(raw("https://github.com/o/r/blob/main"), None);
    }

    #[test]
    fn only_plain_http_urls() {
        assert!(http_url("file:///etc/passwd").is_err());
        assert!(http_url("https://user:pw@example.com/").is_err());
        assert!(http_url("https://example.com/").is_ok());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let mut s = "aé".repeat(4);
        assert!(truncate(&mut s, 4));
        assert_eq!(s, "aéa");
    }
}
