//! The tray icon on Linux, as a StatusNotifierItem of its own.
//!
//! Tauri's tray on Linux is libayatana-appindicator, which offers a menu and nothing else: it
//! ignores `Activate`, the call a tray host makes on a left click, so a left click on t3boat's
//! icon did nothing (Omarchy's bar, KDE, and others send it). This item answers `Activate` by
//! opening the panel, as a click on the menu bar icon does on macOS, and keeps the menu for a
//! right click.

use ksni::TrayMethods;
use std::sync::OnceLock;
use tauri::AppHandle;

/// What the tray does to the app; given by `tray.rs`, which owns the panel.
#[derive(Clone, Copy)]
pub struct Actions {
    /// A left click, and "Open t3boat": shows the panel, or hides it when it is up.
    pub toggle: fn(&AppHandle),
}

struct Item {
    handle: AppHandle,
    actions: Actions,
    icon: Vec<ksni::Icon>,
    tooltip: String,
}

static TRAY: OnceLock<ksni::Handle<Item>> = OnceLock::new();

impl Item {
    /// Runs `f` on the app's main thread: tray calls arrive on a D-Bus task.
    fn on_main(&self, f: fn(&AppHandle)) {
        let handle = self.handle.clone();
        let _ = self.handle.run_on_main_thread(move || f(&handle));
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "t3boat".into()
    }

    fn title(&self) -> String {
        "t3boat".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.tooltip.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.on_main(self.actions.toggle);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Open t3boat".into(),
                activate: Box::new(|item: &mut Self| item.on_main(item.actions.toggle)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit t3boat".into(),
                activate: Box::new(|item: &mut Self| item.on_main(|h| h.exit(0))),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// The icon as the StatusNotifierItem spec wants it: ARGB32, network byte order.
fn pixmap(png: &[u8]) -> Vec<ksni::Icon> {
    let Ok(image) = tauri::image::Image::from_bytes(png) else { return Vec::new() };
    let data = image.rgba().chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
    vec![ksni::Icon { width: image.width() as i32, height: image.height() as i32, data }]
}

/// Puts t3boat's icon in the tray. A tray host that starts later still finds it.
pub fn spawn(handle: &AppHandle, icon_png: &[u8], actions: Actions) {
    let item = Item { handle: handle.clone(), actions, icon: pixmap(icon_png), tooltip: "t3boat".into() };
    tauri::async_runtime::spawn(async move {
        match item.spawn().await {
            Ok(tray) => {
                let _ = TRAY.set(tray);
            }
            Err(e) => tracing::warn!("tray icon: {e}"),
        }
    });
}

/// Shows `tip` when the pointer rests on the icon.
pub fn set_tooltip(tip: String) {
    let Some(tray) = TRAY.get().cloned() else { return };
    tauri::async_runtime::spawn(async move {
        tray.update(|item| item.tooltip = tip).await;
    });
}
