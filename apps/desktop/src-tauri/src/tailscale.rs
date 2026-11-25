//! Tailscale Serve: puts each machine's proxy on this computer's tailnet name, at the same port
//! number, over HTTPS. A phone on the tailnet then pairs with a machine by scanning a QR code.

use crate::process::command;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// The CLI inside the Mac App Store and standalone apps, which put nothing on PATH.
const MACOS_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
/// `tailscale serve` waits for ever when Serve is off for the tailnet.
const TIMEOUT: Duration = Duration::from_secs(20);

/// One Serve change at a time: each rewrites the whole Serve config, and concurrent ones conflict.
static SERVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `tailscale` CLI: on PATH or the login shell's PATH, else in the macOS app.
pub async fn locate() -> Option<PathBuf> {
    if let Some(bin) = crate::mcp::locate("tailscale").await {
        return Some(bin);
    }
    let app = Path::new(MACOS_APP_CLI);
    (cfg!(target_os = "macos") && app.is_file()).then(|| app.to_owned())
}

/// Runs `tailscale args…` and returns its stdout.
async fn run(args: &[&str]) -> Result<Vec<u8>> {
    let bin = locate().await.context("Tailscale is not installed on this computer")?;
    let shown = format!("tailscale {}", args.join(" "));
    let out = command(&bin).args(args).stdin(Stdio::null()).kill_on_drop(true).output();
    let out = tokio::time::timeout(TIMEOUT, out)
        .await
        .map_err(|_| anyhow!("{shown} did not finish: turn on HTTPS and Serve for your tailnet in the Tailscale admin console"))?
        .with_context(|| format!("running {shown}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_owned();
        bail!("{shown} failed: {text}");
    }
    Ok(out.stdout)
}

/// This computer's MagicDNS name, as `tailscale status` gives it. `None`: Tailscale is missing,
/// stopped, or has no MagicDNS name.
pub async fn detect_host() -> Option<String> {
    parse_dns_name(&run(&["status", "--json"]).await.ok()?)
}

/// `Self.DNSName` of `tailscale status --json`, normalized.
pub fn parse_dns_name(status_json: &[u8]) -> Option<String> {
    let status: serde_json::Value = serde_json::from_slice(status_json).ok()?;
    crate::store::normalize_tailscale_host(status.get("Self")?.get("DNSName")?.as_str()?)
}

/// Serves `https://<tailnet name>:<port>/` from the proxy at `127.0.0.1:<port>`, checking
/// `still_wanted` under the Serve lock right before running so a queued enable cannot recreate a
/// port that was deleted or disabled meanwhile. Returns `Ok(true)` if the command ran, `Ok(false)`
/// if skipped.
pub async fn expose_port_if(port: u16, still_wanted: impl FnOnce() -> bool) -> Result<bool> {
    let _one = SERVE.lock().await;
    if !still_wanted() {
        return Ok(false);
    }
    run(&["serve", "--bg", &format!("--https={port}"), "--set-path=/", &format!("http://127.0.0.1:{port}")]).await?;
    Ok(true)
}

/// Stops serving the root handler `/` on `port` on the tailnet, checking `still_wanted` under the
/// Serve lock. Using `--set-path=/` targets only the boat's proxy handler without prompting.
/// Returns `Ok(true)` when removed (or already absent), `Ok(false)` if skipped.
pub async fn unexpose_port_if(port: u16, still_wanted: impl FnOnce() -> bool) -> Result<bool> {
    let _one = SERVE.lock().await;
    if !still_wanted() {
        return Ok(false);
    }
    match run(&["serve", &format!("--https={port}"), "--set-path=/", "off"]).await {
        Ok(_) => Ok(true),
        Err(e) if is_absent_handler_error(&e.to_string()) => Ok(true),
        Err(e) => Err(e),
    }
}

/// Tailscale Serve returns an error when removing a handler that is already absent.
/// Narrowed to handler-absent diagnostics so HTTP `404 Not Found` from the local API still retries.
fn is_absent_handler_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("handler does not exist") || lower.contains("no serve config")
}

/// The page a device on the tailnet opens to pair with the machine served at `port`.
pub fn tailnet_pairing_url(host: &str, port: u16, code: &str) -> String {
    format!("https://{host}:{port}/pair#token={}", urlencoding::encode(code))
}

/// `text` as a QR code: an `<svg>` element, dark on white with its quiet zone, one unit per
/// module. Scales without blur.
pub fn qr_svg(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    let svg = code.render::<qrcode::render::svg::Color>().module_dimensions(1, 1).quiet_zone(true).build();
    // The XML prolog is for files; inline the `<svg>` alone.
    let start = svg.find("<svg").context("the QR renderer gave no <svg>")?;
    Ok(svg[start..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_magicdns_name_from_status() {
        let status = br#"{"Version": "1.80.0", "Self": {"HostName": "MyBox", "DNSName": "MyBox.tail1234.ts.net.", "TailscaleIPs": ["100.64.0.1"]}}"#;
        assert_eq!(parse_dns_name(status).as_deref(), Some("mybox.tail1234.ts.net"));
        for bad in [
            &br#"{"Self": {"DNSName": ""}}"#[..],
            br#"{"Self": {"DNSName": "mybox.example.com."}}"#,
            br#"{"Self": {}}"#,
            br#"{"BackendState": "Stopped"}"#,
            b"not json",
        ] {
            assert_eq!(parse_dns_name(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn pairing_url_carries_the_code_in_the_fragment() {
        assert_eq!(
            tailnet_pairing_url("mybox.tail1234.ts.net", 47731, "abc_-123"),
            "https://mybox.tail1234.ts.net:47731/pair#token=abc_-123"
        );
        assert_eq!(tailnet_pairing_url("h.t.ts.net", 1, "a+b/c="), "https://h.t.ts.net:1/pair#token=a%2Bb%2Fc%3D");
    }

    #[test]
    fn qr_svg_is_a_bare_svg_element() {
        let svg = qr_svg("https://mybox.tail1234.ts.net:47731/pair#token=abc").unwrap();
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""), "{svg}");
        assert!(svg.ends_with("</svg>"));
        assert!(!svg.contains("<?xml"));
        assert!(svg.contains("fill=\"#fff\"") && svg.contains("fill=\"#000\""));
        // Version 3+ at one unit per module, quiet zone of 4 on each side: at least 29 + 8 wide.
        let width: u32 = svg.split("width=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        assert!(width >= 37, "{width}");
    }
}
