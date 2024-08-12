mod client;
mod message;
mod reverse;
pub mod server;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REV: &str = env!("REPO_REV");
pub const DIRTY: &str = env!("REPO_DIRTY");

pub use client::run_client;
pub use reverse::browse_url;
pub use reverse::clip_file;
pub use server::run_server;
