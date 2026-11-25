pub mod app;
pub mod boat;
pub mod github;
pub mod machine;
#[cfg(all(feature = "desktop", target_os = "linux"))]
mod linux_tray;
pub mod mcp;
pub mod mothership;
#[cfg(feature = "desktop")]
mod omarchy;
pub mod process;
pub mod projects;
pub mod proxy;
pub mod store;
pub mod t3desktop;
pub mod tailscale;
#[cfg(feature = "desktop")]
mod tray;

#[cfg(feature = "desktop")]
pub use tray::run;
