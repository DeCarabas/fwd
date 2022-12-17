use crate::message::{Message, MessageReader};
use anyhow::{bail, Context, Result};
use log::warn;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use users;
use xdg;

pub async fn handle_browser_open_impl(
    messages: mpsc::Sender<Message>,
) -> Result<()> {
    let path = socket_path().context("Error getting socket path")?;
    handle_browser_open_with_path(messages, path).await
}

async fn handle_browser_open_with_path(
    messages: mpsc::Sender<Message>,
    path: PathBuf,
) -> Result<()> {
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("Failed to bind to {}", path.display()))?;
    loop {
        let (socket, _addr) = listener
            .accept()
            .await
            .context("Error accepting connection")?;

        let sender = messages.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, sender).await {
                warn!("Error handling socket connection: {:?}", e);
            }
        });
    }
}

pub fn socket_path() -> Result<PathBuf> {
    let mut socket_path = socket_directory()?;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&socket_path)
        .context("Error creating socket directory")?;

    // TODO: check mode of directory

    socket_path.push("browser");
    Ok(socket_path)
}

fn socket_directory() -> Result<std::path::PathBuf> {
    let base_directories = xdg::BaseDirectories::new()
        .context("Error creating BaseDirectories")?;
    match base_directories.place_runtime_file("fwd") {
        Ok(path) => Ok(path),
        Err(_) => {
            let mut path = std::env::temp_dir();
            path.push(format!("fwd{}", users::get_current_uid()));
            Ok(path)
        }
    }
}

async fn handle_connection(
    socket: UnixStream,
    sender: mpsc::Sender<Message>,
) -> Result<()> {
    let mut reader = MessageReader::new(socket);
    let message = reader.read().await.context("Error reading message")?;
    match message {
        Message::Browse(url) => sender.send(Message::Browse(url)).await?,
        _ => bail!("Unsupported message: {:?}", message),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageWriter;
    use tempdir::TempDir;

    #[test]
    fn socket_path_repeats() {
        assert_eq!(
            socket_path().expect("Could not get socket path a"),
            socket_path().expect("Could not get socket path b")
        );
    }

    #[tokio::test]
    async fn url_to_message() {
        let (sender, mut receiver) = mpsc::channel(64);

        let tmp_dir =
            TempDir::new("url_to_message").expect("Error getting tmpdir");
        let path = tmp_dir.path().join("socket");

        let path_override = path.clone();
        tokio::spawn(async move {
            handle_browser_open_with_path(sender, path_override)
                .await
                .expect("Error in server!");
        });

        let mut attempt = 0;
        let stream = loop {
            match UnixStream::connect(&path).await {
                Ok(stream) => break Ok(stream),
                Err(e) => {
                    if attempt == 5 {
                        break Err(e)
                            .context("Maximum retries exceeded, last error");
                    }
                    attempt += 1;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        .expect("Error connecting to socket");
        let mut writer = MessageWriter::new(stream);
        let sent = Message::Browse("https://google.com/".to_string());
        writer
            .write(sent.clone())
            .await
            .expect("Error writing browse message");

        let received = receiver.recv().await.expect("Error receiving message");
        assert_eq!(sent, received);
    }
}
