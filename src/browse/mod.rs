use crate::message::Message;
use anyhow::Result;
use tokio::sync::mpsc;

#[cfg(target_family = "unix")]
mod browse_unix;

#[cfg(target_family = "unix")]
use browse_unix::{browse_url_impl, handle_browser_open_impl};

#[inline]
pub async fn browse_url(url: &String) {
    browse_url_impl(url).await
}

#[cfg(not(target_family = "unix"))]
pub async fn browse_url_impl(url: &String) {
    print!("Opening a browser is not supported on this platform\n");
    std::process::exit(1);
}

#[inline]
pub async fn handle_browser_open(
    messages: mpsc::Sender<Message>,
) -> Result<()> {
    handle_browser_open_impl(messages).await
}

#[cfg(not(target_family = "unix"))]
async fn handle_browser_open_impl(
    messages: mpsc::Sender<Message>,
) -> Result<()> {
    std::future::pending().await
}