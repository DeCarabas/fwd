use bytes::BytesMut;
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tokio::process;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

mod message;
mod refresh;

use message::Message;

struct MessageWriter<T: AsyncWrite + Unpin> {
    writer: T,
}

impl<T: AsyncWrite + Unpin> MessageWriter<T> {
    fn new(writer: T) -> MessageWriter<T> {
        MessageWriter { writer }
    }
    async fn write(self: &mut Self, msg: Message) -> Result<(), tokio::io::Error> {
        // TODO: Optimize buffer usage please this is bad
        let mut buffer = msg.encode();
        self.writer
            .write_u32(buffer.len().try_into().expect("Message too large"))
            .await?;
        self.writer.write_buf(&mut buffer).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

async fn pump_write<T: AsyncWrite + Unpin>(
    messages: &mut mpsc::Receiver<Message>,
    writer: &mut MessageWriter<T>,
) -> Result<(), tokio::io::Error> {
    while let Some(msg) = messages.recv().await {
        writer.write(msg).await?;
    }
    Ok(())
}

async fn server_read<T: AsyncRead + Unpin>(
    reader: &mut T,
    writer: mpsc::Sender<Message>,
) -> Result<(), tokio::io::Error> {
    eprintln!("< Processing packets...");
    loop {
        let frame_length = reader.read_u32().await?;

        let mut data = BytesMut::with_capacity(frame_length.try_into().unwrap());
        reader.read_buf(&mut data).await?;

        let mut cursor = Cursor::new(&data[..]);
        let message = match Message::decode(&mut cursor) {
            Ok(msg) => msg,
            Err(_) => return Err(tokio::io::Error::from(tokio::io::ErrorKind::InvalidData)),
        };

        use Message::*;
        match message {
            Ping => (),
            Refresh => {
                let writer = writer.clone();
                tokio::spawn(async move {
                    let ports = match refresh::get_entries() {
                        Ok(ports) => ports,
                        Err(e) => {
                            eprintln!("< Error scanning: {:?}", e);
                            vec![]
                        }
                    };
                    if let Err(e) = writer.send(Message::Ports(ports)).await {
                        // Writer has been closed for some reason, we can just quit.... I hope everything is OK?
                        eprintln!("< Warning: Error sending: {:?}", e);
                    }
                });
            }
            _ => panic!("Unsupported: {:?}", message),
        };
    }
}

async fn server_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    reader: &mut Reader,
    writer: &mut MessageWriter<Writer>,
) -> Result<(), tokio::io::Error> {
    // Jump into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = server_read(reader, msg_sender);
    tokio::pin!(reading);
    tokio::pin!(writing);

    let (mut done_writing, mut done_reading) = (false, false);
    loop {
        tokio::select! {
            result = &mut writing, if !done_writing => {
                done_writing = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
            result = &mut reading, if !done_reading => {
                done_reading = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
        }
    }
}

async fn spawn_ssh(server: &str) -> Result<tokio::process::Child, tokio::io::Error> {
    let mut cmd = process::Command::new("ssh");
    cmd.arg("-T").arg(server).arg("fwd").arg("--server");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    cmd.spawn()
}

async fn client_sync<T: AsyncRead + Unpin>(reader: &mut T) -> Result<(), tokio::io::Error> {
    eprintln!("> Waiting for synchronization marker...");
    let mut seen = 0;
    while seen < 8 {
        let byte = reader.read_u8().await?;
        seen = if byte == 0 { seen + 1 } else { 0 };
    }
    Ok(())
}

// struct Connection {
//     connected: oneshot::Sender<Result<(), tokio::io::Error>>,
// }

// struct ConnectionTable {
//     next_id: u64,
//     connections: HashMap<u64, Connection>,
// }

// type Connections = Arc<Mutex<ConnectionTable>>;

async fn client_listen(port: u16) -> Result<(), tokio::io::Error> {
    loop {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
        loop {
            // The second item contains the IP and port of the new connection.
            // TODO: Handle shutdown correctly.
            let (stream, _) = listener.accept().await?;

            eprintln!("> CONNECTION NOT IMPLEMENTED!");
        }
    }
}

async fn client_read<T: AsyncRead + Unpin>(
    reader: &mut T,
    _writer: mpsc::Sender<Message>,
) -> Result<(), tokio::io::Error> {
    let mut listeners: HashMap<u16, oneshot::Sender<()>> = HashMap::new();

    eprintln!("> Processing packets...");
    loop {
        let frame_length = reader.read_u32().await?;

        let mut data = BytesMut::with_capacity(frame_length.try_into().unwrap());
        reader.read_buf(&mut data).await?;

        let mut cursor = Cursor::new(&data[..]);
        let message = match Message::decode(&mut cursor) {
            Ok(msg) => msg,
            Err(_) => return Err(tokio::io::Error::from(tokio::io::ErrorKind::InvalidData)),
        };

        use Message::*;
        match message {
            Ping => (),
            Ports(ports) => {
                let mut new_listeners = HashMap::new();

                println!("The following ports are available:");
                for port in ports {
                    println!("  {}: {}", port.port, port.desc);

                    let port = port.port;
                    if let Some(l) = listeners.remove(&port) {
                        if !l.is_closed() {
                            // Listen could have failed!
                            new_listeners.insert(port, l);
                        }
                    }

                    if !new_listeners.contains_key(&port) {
                        let (l, stop) = oneshot::channel();
                        new_listeners.insert(port, l);

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                r = client_listen(port) => r,
                                _ = stop => Ok(()),
                            };
                            if let Err(e) = result {
                                eprintln!("> Error listening on port {}: {:?}", port, e);
                            }
                        });
                    }
                }

                listeners = new_listeners;
            }
            _ => panic!("Unsupported: {:?}", message),
        };
    }
}

async fn client_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    reader: &mut Reader,
    writer: &mut MessageWriter<Writer>,
) -> Result<(), tokio::io::Error> {
    // First synchronize; we're looking for the 8-zero marker that is the 64b sync marker.
    // This helps us skip garbage like any kind of MOTD or whatnot.
    client_sync(reader).await?;

    // Now kick things off with a listing of the ports...
    eprintln!("> Sending initial list command...");
    writer.write(Message::Refresh).await?;

    // And now really get into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = client_read(reader, msg_sender);
    tokio::pin!(reading);
    tokio::pin!(writing);

    let (mut done_writing, mut done_reading) = (false, false);
    loop {
        tokio::select! {
            result = &mut writing, if !done_writing => {
                done_writing = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
            result = &mut reading, if !done_reading => {
                done_reading = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let remote = &args[1];
    if remote == "--server" {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut writer = BufWriter::new(tokio::io::stdout());

        // Write the marker.
        eprintln!("< Writing marker...");
        writer
            .write_u64(0x00_00_00_00_00_00_00_00)
            .await
            .expect("Error writing marker");

        writer.flush().await.expect("Error flushing buffer");
        eprintln!("< Done!");

        let mut writer = MessageWriter::new(writer);

        if let Err(e) = server_main(&mut reader, &mut writer).await {
            eprintln!("Error: {:?}", e);
        }
    } else {
        let mut child = spawn_ssh(remote).await.expect("failed to spawn");

        let mut writer = MessageWriter::new(BufWriter::new(
            child
                .stdin
                .take()
                .expect("child did not have a handle to stdout"),
        ));

        let mut reader = BufReader::new(
            child
                .stdout
                .take()
                .expect("child did not have a handle to stdout"),
        );

        if let Err(e) = client_main(&mut reader, &mut writer).await {
            eprintln!("Error: {:?}", e);
        }
    }
}
