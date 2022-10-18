use crate::message::{Message, MessageReader, MessageWriter};
use anyhow::Result;
use log::{error, warn};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

mod refresh;

async fn server_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    reader: &mut MessageReader<Reader>,
    writer: &mut MessageWriter<Writer>,
) -> Result<()> {
    // The first message we send must be an announcement.
    writer.write(Message::Hello(0, 1, vec![])).await?;

    loop {
        use Message::*;
        match reader.read().await? {
            Ping => (),
            Refresh => {
                let ports = match refresh::get_entries() {
                    Ok(ports) => ports,
                    Err(e) => {
                        error!("Error scanning: {:?}", e);
                        vec![]
                    }
                };
                if let Err(e) = writer.write(Message::Ports(ports)).await {
                    // Writer has been closed for some reason, we can just
                    // quit.... I hope everything is OK?
                    warn!("Warning: Error sending: {:?}", e);
                }
            }
            message => panic!("Unsupported: {:?}", message),
        };
    }
}

pub async fn run_server() {
    let reader = BufReader::new(tokio::io::stdin());
    let mut writer = BufWriter::new(tokio::io::stdout());

    // Write the 8-byte synchronization marker.
    writer
        .write_u64(0x00_00_00_00_00_00_00_00)
        .await
        .expect("Error writing marker");

    if let Err(e) = writer.flush().await {
        eprintln!("Error writing sync marker: {:?}", e);
        return;
    }

    let mut writer = MessageWriter::new(writer);
    let mut reader = MessageReader::new(reader);
    if let Err(e) = server_main(&mut reader, &mut writer).await {
        eprintln!("Error: {:?}", e);
    }
}
