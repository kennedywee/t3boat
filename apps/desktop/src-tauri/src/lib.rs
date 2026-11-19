pub mod app;
pub mod boat;
pub mod github;
pub mod machine;
#[cfg(target_os = "linux")]
mod linux_tray;
pub mod mcp;
mod omarchy;
pub mod process;
pub mod projects;
pub mod proxy;
pub mod store;
pub mod t3desktop;
mod tray;

pub use tray::run;
