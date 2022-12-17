use crate::message::Message;
use anyhow::Result;
use tokio::sync::mpsc;

#[cfg(target_family = "unix")]
use browser_unix::handle_browser_open_impl;

mod browser_unix;

#[inline]
pub async fn handle_browser_open(
    messages: mpsc::Sender<Message>,
) -> Result<()> {
    handle_browser_open_impl(messages).await
}
