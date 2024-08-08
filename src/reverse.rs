use anyhow::Result;
use rand::random;
use tokio::io::{AsyncRead, AsyncReadExt};

#[cfg(target_family = "unix")]
mod unix;

#[cfg(target_family = "unix")]
pub use unix::{handle_reverse_connections, ReverseConnection};

use crate::message::Message;

#[cfg(not(target_family = "unix"))]
pub struct ReverseConnection {}

#[cfg(not(target_family = "unix"))]
impl ReverseConnection {
    pub async fn new() -> Result<Self> {
        use anyhow::anyhow;
        Err(anyhow!(
            "Server-side operations are not supported on this platform"
        ))
    }

    pub async fn send(&mut self, message: Message) -> Result<()> {
        use anyhow::anyhow;
        Err(anyhow!(
            "Server-side operations are not supported on this platform"
        ))
    }
}

#[cfg(not(target_family = "unix"))]
pub async fn handle_reverse_connections(
    _messages: tokio::sync::mpsc::Sender<Message>,
) -> Result<()> {
    std::future::pending().await
}

#[inline]
pub async fn browse_url(url: &str) -> Result<()> {
    ReverseConnection::new()
        .await?
        .send(Message::Browse(url.to_string()))
        .await
}

async fn clip_reader<T: AsyncRead + Unpin>(reader: &mut T) -> Result<()> {
    let mut connection = ReverseConnection::new().await?;
    let clip_id: u64 = random();
    connection.send(Message::ClipStart(clip_id)).await?;

    let mut count = 0;
    let mut buf = vec![0; 1024];
    loop {
        let read = reader.read(&mut buf[count..]).await?;
        if read == 0 {
            break;
        }
        count += read;
        if count == buf.len() {
            connection.send(Message::ClipData(clip_id, buf)).await?;
            buf = vec![0; 1024];
            count = 0;
        }
    }

    if count > 0 {
        buf.resize(count, 0);
        connection.send(Message::ClipData(clip_id, buf)).await?;
    }

    connection.send(Message::ClipEnd(clip_id)).await?;
    Ok(())
}

#[inline]
pub async fn clip_file(file: &str) -> Result<()> {
    if file == "-" {
        let mut stdin = tokio::io::stdin();
        clip_reader(&mut stdin).await?;
    } else {
        let mut file = tokio::fs::File::open(file).await?;
        clip_reader(&mut file).await?;
    }

    Ok(())
}
