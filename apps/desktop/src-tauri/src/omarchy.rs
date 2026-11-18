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

use std::path::Path;

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
