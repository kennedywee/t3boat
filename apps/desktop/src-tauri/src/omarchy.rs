//! Omarchy support: Hyprland as the compositor, often on NVIDIA.
//!
//! Kept in one module, with small hooks in `tray.rs`, so this fork merges upstream changes easily.
//!
//! - WebKitGTK's DMABUF renderer fails on NVIDIA: on Wayland the app dies with "Error 71
//!   (Protocol error) dispatching to Wayland display" as the panel first draws, and under
//!   XWayland it draws nothing ("Failed to create GBM buffer").
//! - Hyprland places windows itself (a client cannot set its position on Wayland), and Omarchy
//!   turns on focus-follows-mouse, so a panel that hides when it loses focus hides as soon as the
//!   pointer leaves it. There the panel is an ordinary floating window instead, resizable, and
//!   hidden from the tray icon or by closing the window.

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::Emitter;

/// The panel's size from `tauri.conf.json`, in logical pixels.
const PANEL_SIZE: (f64, f64) = (380.0, 560.0);

/// Whether t3boat runs under Hyprland, Omarchy's compositor.
pub fn hyprland() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

fn nvidia() -> bool {
    Path::new("/proc/driver/nvidia").exists()
}

/// Sets the environment WebKitGTK needs on this machine. Runs first in `run`, before GTK or
/// WebKit start and before any other thread exists. A value the user set is kept.
pub fn prepare_env() {
    if !cfg!(target_os = "linux") {
        return;
    }
    if nvidia() && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: called at the start of `run`, while the process has one thread.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
}

/// Opens the panel at its own size instead of Hyprland's default floating size. Hyprland keeps
/// a fixed-size window at its size as it maps, so the panel starts fixed: see `release_size`.
pub fn fit_panel(panel: &tauri::WebviewWindow) {
    if !hyprland() {
        return;
    }
    let size = tauri::LogicalSize::new(PANEL_SIZE.0, PANEL_SIZE.1);
    let _ = panel.set_min_size(Some(size));
    let _ = panel.set_max_size(Some(size));
    let _ = panel.set_size(size);
}

/// Once the panel is on screen at its size, lets it be resized or tiled: the UI follows the
/// window's width. Hyprland keeps the size it mapped with.
pub fn release_size(panel: &tauri::WebviewWindow) {
    if !hyprland() {
        return;
    }
    let panel = panel.clone();
    std::thread::spawn(move || {
        // The show is queued on GTK's main loop; give the compositor time to map the window.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = panel.set_max_size(None::<tauri::Size>);
        let _ = panel.set_resizable(true);
    });
}

// ── Theme ─────────────────────────────────────────────────────────────
//
// Omarchy applies a theme by moving it to ~/.local/state/omarchy/current/theme and writing a new
// `theme.generation`. Its `colors.toml` is the palette every themed app is made from (T3 Code's
// own `t3code.json` among them), so t3boat takes its colours from there too and follows each
// `omarchy theme set` while it runs.

/// How often the theme generation is checked; a theme change shows up within this.
const THEME_POLL: Duration = Duration::from_secs(2);

/// The current Omarchy theme, as the UI applies it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    pub name: String,
    /// "dark" or "light"
    pub mode: String,
    /// `colors.toml`'s colours by name: background, foreground, accent, blue, …
    pub colors: BTreeMap<String, String>,
    /// The monospace font Omarchy sets (fontconfig's `monospace`).
    pub font: Option<String>,
}

fn state_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".local/state/omarchy/current"))
}

/// Reads `key = "value"` lines: the flat shape of Omarchy's `colors.toml`.
fn parse_colors(text: &str) -> (Option<String>, BTreeMap<String, String>) {
    let mut mode = None;
    let mut colors = BTreeMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        let Some((key, rest)) = line.split_once('=') else { continue };
        // The quoted value; a comment may follow it, and colours themselves start with '#'.
        let Some(value) = rest.trim().strip_prefix('"').and_then(|v| v.split('"').next()) else { continue };
        let key = key.trim();
        if key == "mode" {
            mode = Some(value.to_owned());
        } else if value.len() == 7 && value.starts_with('#') && value[1..].chars().all(|c| c.is_ascii_hexdigit()) {
            colors.insert(key.to_owned(), value.to_ascii_lowercase());
        }
    }
    (mode, colors)
}

fn monospace_font() -> Option<String> {
    let out = std::process::Command::new("fc-match").args(["monospace", "-f", "%{family}"]).output().ok()?;
    let family = String::from_utf8_lossy(&out.stdout);
    let first = family.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

/// The theme Omarchy applied; `None` off Omarchy or when its palette can't be read.
pub fn current_theme() -> Option<Theme> {
    let dir = state_dir()?;
    let text = std::fs::read_to_string(dir.join("theme/colors.toml")).ok()?;
    let (mode, colors) = parse_colors(&text);
    // Too few colours to theme with: leave t3boat's own look.
    if !["background", "foreground", "accent"].iter().all(|k| colors.contains_key(*k)) {
        return None;
    }
    let name = std::fs::read_to_string(dir.join("theme.name")).unwrap_or_default().trim().to_owned();
    Some(Theme { name, mode: mode.unwrap_or_else(|| "dark".into()), colors, font: monospace_font() })
}

fn generation() -> Option<String> {
    std::fs::read_to_string(state_dir()?.join("theme.generation")).ok()
}

/// Tells the UI ("omarchy-theme") whenever Omarchy applies another theme.
pub fn watch_theme(handle: tauri::AppHandle) {
    if !cfg!(target_os = "linux") || state_dir().is_none_or(|d| !d.exists()) {
        return;
    }
    std::thread::spawn(move || {
        let mut seen = generation();
        loop {
            std::thread::sleep(THEME_POLL);
            let now = generation();
            if now != seen {
                seen = now;
                let _ = handle.emit("omarchy-theme", current_theme());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::parse_colors;

    #[test]
    fn reads_omarchy_colors() {
        let (mode, colors) = parse_colors(
            "# Vault\nmode = \"dark\"\n\naccent = \"#E09355\"\nbackground = \"#191510\" # ground\n\
             hyprland_active_border = \"rgba(e09355ee) rgba(d9ab5eee) 45deg\"\nbad = \"#12\"\n",
        );
        assert_eq!(mode.as_deref(), Some("dark"));
        assert_eq!(colors.get("accent").map(String::as_str), Some("#e09355"));
        assert_eq!(colors.get("background").map(String::as_str), Some("#191510"));
        assert!(!colors.contains_key("hyprland_active_border") && !colors.contains_key("bad"));
    }
}
