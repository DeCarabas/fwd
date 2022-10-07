use bytes::BytesMut;
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, Error, ErrorKind,
};
use tokio::net::{TcpListener, TcpStream};
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
    async fn write(self: &mut Self, msg: Message) -> Result<(), Error> {
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
) -> Result<(), Error> {
    while let Some(msg) = messages.recv().await {
        writer.write(msg).await?;
    }
    Ok(())
}

async fn server_connect(
    channel: u64,
    port: u16,
    writer: &mut mpsc::Sender<Message>,
) -> Result<(), Error> {
    let _stream = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
    if let Err(e) = writer.send(Message::Connected(channel, port)).await {
        eprintln!("< Warning: couldn't send Connected: {:?}", e);
        return Err(Error::from(ErrorKind::BrokenPipe));
    }

    // Do the thing, read and write and whatnot.

    Ok(())
}

async fn server_read<T: AsyncRead + Unpin>(
    reader: &mut T,
    writer: mpsc::Sender<Message>,
) -> Result<(), Error> {
    eprintln!("< Processing packets...");
    loop {
        let frame_length = reader.read_u32().await?;

        let mut data = BytesMut::with_capacity(frame_length.try_into().unwrap());
        reader.read_buf(&mut data).await?;

        let mut cursor = Cursor::new(&data[..]);
        let message = match Message::decode(&mut cursor) {
            Ok(msg) => msg,
            Err(_) => return Err(Error::from(ErrorKind::InvalidData)),
        };

        use Message::*;
        match message {
            Ping => (),
            Connect(channel, port) => {
                let writer = writer.clone();
                tokio::spawn(async move {
                    let mut writer = writer;
                    if let Err(e) = server_connect(channel, port, &mut writer).await {
                        eprintln!("< Connection failed: {:?}", e);
                        _ = writer.send(Message::Abort(channel)).await;
                    }
                });
            }
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
) -> Result<(), Error> {
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

async fn spawn_ssh(server: &str) -> Result<tokio::process::Child, Error> {
    let mut cmd = process::Command::new("ssh");
    cmd.arg("-T").arg(server).arg("fwd").arg("--server");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    cmd.spawn()
}

async fn client_sync<T: AsyncRead + Unpin>(reader: &mut T) -> Result<(), Error> {
    eprintln!("> Waiting for synchronization marker...");
    let mut seen = 0;
    while seen < 8 {
        let byte = reader.read_u8().await?;
        seen = if byte == 0 { seen + 1 } else { 0 };
    }
    Ok(())
}

struct ClientConnection {
    connected: Option<oneshot::Sender<Result<(), Error>>>,
}

struct ClientConnectionTable {
    next_id: u64,
    connections: HashMap<u64, ClientConnection>,
}

impl ClientConnectionTable {
    fn new() -> ClientConnectionTable {
        ClientConnectionTable {
            next_id: 0,
            connections: HashMap::new(),
        }
    }
}

type ClientConnections = Arc<Mutex<ClientConnectionTable>>;

async fn client_handle_connection(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ClientConnections,
) {
    let (connected, rx) = oneshot::channel();
    let channel_id = {
        let mut tbl = connections.lock().unwrap();
        let id = tbl.next_id;
        tbl.next_id += 1;
        tbl.connections.insert(
            id,
            ClientConnection {
                connected: Some(connected),
            },
        );
        id
    };

    if let Ok(_) = writer.send(Message::Connect(channel_id, port)).await {
        if let Ok(r) = rx.await {
            if let Ok(_) = r {
                // Connection worked! Do the damn thing.
                eprintln!("Got here I guess! {}", channel_id);
            }
        }
    }

    {
        let mut tbl = connections.lock().unwrap();
        tbl.connections.remove(&channel_id);
    }
    // If the writer is closed then the whole connection is closed.
    _ = writer.send(Message::Close(channel_id)).await;
}

async fn client_listen(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ClientConnections,
) -> Result<(), Error> {
    loop {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
        loop {
            // The second item contains the IP and port of the new connection.
            // TODO: Handle shutdown correctly.
            let (_, _) = listener.accept().await?;

            let (writer, connections) = (writer.clone(), connections.clone());
            tokio::spawn(async move {
                client_handle_connection(port, writer, connections).await;
            });
        }
    }
}

async fn client_read<T: AsyncRead + Unpin>(
    reader: &mut T,
    connections: ClientConnections,
    writer: mpsc::Sender<Message>,
) -> Result<(), Error> {
    let mut listeners: HashMap<u16, oneshot::Sender<()>> = HashMap::new();

    eprintln!("> Processing packets...");
    loop {
        let frame_length = reader.read_u32().await?;

        let mut data = BytesMut::with_capacity(frame_length.try_into().unwrap());
        reader.read_buf(&mut data).await?;

        let mut cursor = Cursor::new(&data[..]);
        let message = match Message::decode(&mut cursor) {
            Ok(msg) => msg,
            Err(_) => return Err(Error::from(ErrorKind::InvalidData)),
        };

        use Message::*;
        match message {
            Ping => (),
            Connected(channel, _) => {
                let connected = {
                    let mut tbl = connections.lock().unwrap();
                    if let Some(c) = tbl.connections.get_mut(&channel) {
                        c.connected.take()
                    } else {
                        None
                    }
                };

                if let Some(connected) = connected {
                    // If we can't send the notification then... uh... ok?
                    _ = connected.send(Ok(()));
                }
            }
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

                        let (writer, connections) = (writer.clone(), connections.clone());
                        tokio::spawn(async move {
                            let result = tokio::select! {
                                r = client_listen(port, writer, connections) => r,
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
) -> Result<(), Error> {
    // First synchronize; we're looking for the 8-zero marker that is the 64b sync marker.
    // This helps us skip garbage like any kind of MOTD or whatnot.
    client_sync(reader).await?;

    // Now kick things off with a listing of the ports...
    eprintln!("> Sending initial list command...");
    writer.write(Message::Refresh).await?;

    let connections = Arc::new(Mutex::new(ClientConnectionTable::new()));

    // And now really get into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = client_read(reader, connections, msg_sender);
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
