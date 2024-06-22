use anyhow::Result;

#[cfg(target_family = "unix")]
mod unix;

#[cfg(target_family = "unix")]
pub use unix::{handle_reverse_connections, send_reverse_message};

use crate::message::Message;

#[cfg(not(target_family = "unix"))]
pub async fn send_reverse_message(_message: Message) -> Result<()> {
    use anyhow::anyhow;
    Err(anyhow!(
        "Server-side operations are not supported on this platform"
    ))
}

#[cfg(not(target_family = "unix"))]
pub async fn handle_reverse_connections(
    _messages: mpsc::Sender<Message>,
) -> Result<()> {
    std::future::pending().await
}

#[inline]
pub async fn browse_url(url: &str) -> Result<()> {
    send_reverse_message(Message::Browse(url.to_string())).await
}
