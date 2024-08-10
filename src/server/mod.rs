use crate::message::{Message, MessageReader, MessageWriter};
use crate::reverse::handle_reverse_connections;
use anyhow::Result;
use log::{error, warn};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;

mod refresh;

// We drive writes through an mpsc queue, because we not only handle requests
// and responses from the client (refresh ports and the like) but also need
// to asynchronously send messages to the client (open this URL, etc).
async fn write_driver<Writer: AsyncWrite + Unpin>(
    messages: &mut mpsc::Receiver<Message>,
    writer: &mut MessageWriter<Writer>,
) {
    while let Some(m) = messages.recv().await {
        writer.write(m).await.expect("Failed to write the message")
    }
}

// Handle messages that the client sends to us.
async fn server_loop<Reader: AsyncRead + Unpin>(
    reader: &mut MessageReader<Reader>,
    writer: &mut mpsc::Sender<Message>,
) -> Result<()> {
    // The first message we send must be an announcement.
    writer.send(Message::Hello(0, 2, vec![])).await?;
    let mut version_reported = false;
    loop {
        use Message::*;
        match reader.read().await? {
            Ping => (),
            Refresh => {
                // Just log the version, if we haven't yet. We do this extra
                // work to avoid spamming the log, but we wait until we
                // receive the first message to be sure that the client is in
                // a place to display our logging properly.
                if !version_reported {
                    eprintln!(
                        "fwd server {} (rev {}{})",
                        crate::VERSION,
                        crate::REV,
                        crate::DIRTY
                    );
                    version_reported = true;
                }

                let ports = match refresh::get_entries().await {
                    Ok(ports) => ports,
                    Err(e) => {
                        error!("Error scanning: {:?}", e);
                        vec![]
                    }
                };
                if let Err(e) = writer.send(Message::Ports(ports)).await {
                    // Writer has been closed for some reason, we can just
                    // quit.... I hope everything is OK?
                    warn!("Warning: Error sending: {:?}", e);
                }
            }
            message => panic!("Unsupported: {:?}", message),
        };
    }
}

// Run the various server loops.
async fn server_main<
    In: AsyncRead + Unpin + Send,
    Out: AsyncWrite + Unpin + Send,
>(
    stdin: In,
    stdout: Out,
) -> Result<()> {
    let reader = BufReader::new(stdin);
    let mut writer = BufWriter::new(stdout);

    // Write the 8-byte synchronization marker.
    writer
        .write_u64(0x00_00_00_00_00_00_00_00)
        .await
        .expect("Error writing marker");
    writer.flush().await.expect("Error flushing");

    let (mut sender, mut receiver) = mpsc::channel(10);
    let mut writer = MessageWriter::new(writer);
    let mut reader = MessageReader::new(reader);

    let browse_sender = sender.clone();

    tokio::select! {
        _ = write_driver(&mut receiver, &mut writer) => Ok(()),
        r = server_loop(&mut reader, &mut sender) => r,
        r = handle_reverse_connections(browse_sender) => r,
    }
}

pub async fn run_server() {
    env_logger::Builder::from_env(
        env_logger::Env::new().filter_or("FWD_LOG", "warn"),
    )
    .init();
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    if let Err(e) = server_main(stdin, stdout).await {
        error!("Error: {:?}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use tokio::io::{AsyncReadExt, DuplexStream};

    async fn sync(client_read: &mut DuplexStream) {
        println!("[client] Waiting for server sync...");
        for _ in 0..8 {
            let b = client_read
                .read_u8()
                .await
                .expect("Error reading sync byte");
            assert_eq!(b, 0);
        }

        let mut reader = MessageReader::new(client_read);
        println!("[client] Reading first message...");
        let msg = reader.read().await.expect("Error reading first message");
        assert_matches!(msg, Message::Hello(0, 2, _));
    }

    #[tokio::test]
    async fn basic_hello_sync() {
        let (server_read, _client_write) = tokio::io::duplex(4096);
        let (mut client_read, server_write) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            server_main(server_read, server_write)
                .await
                .expect("Error in server!");
        });

        sync(&mut client_read).await;
    }
}
