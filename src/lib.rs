use bytes::{Bytes, BytesMut};
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

// ----------------------------------------------------------------------------
// Message Writing

struct MessageWriter<T: AsyncWrite + Unpin> {
    writer: T,
}

impl<T: AsyncWrite + Unpin> MessageWriter<T> {
    fn new(writer: T) -> MessageWriter<T> {
        MessageWriter { writer }
    }
    async fn write(self: &mut Self, msg: Message) -> Result<(), Error> {
        // TODO: Optimize buffer usage please this is bad
        // eprintln!("? {:?}", msg);
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

// ----------------------------------------------------------------------------
// Connection

/// Read from a socket and convert the reads into Messages to put into the
/// queue until the socket is closed for reading or an error occurs.
async fn connection_read<T: AsyncRead + Unpin>(
    channel: u64,
    read: &mut T,
    writer: &mut mpsc::Sender<Message>,
) -> Result<(), Error> {
    let result = loop {
        let mut buffer = BytesMut::with_capacity(64 * 1024);
        if let Err(e) = read.read_buf(&mut buffer).await {
            break Err(e);
        }
        if buffer.len() == 0 {
            break Ok(());
        }

        if let Err(_) = writer.send(Message::Data(channel, buffer.into())).await {
            break Err(Error::from(ErrorKind::ConnectionReset));
        }

        // TODO: Flow control here, wait for the packet to be acknowleged so
        // there isn't head-of-line blocking or infinite bufferingon the
        // remote side. Also buffer re-use!
    };

    // We are effectively closed on this side, send the close to drop the
    // corresponding write side on the other end of the pipe.
    _ = writer.send(Message::Close(channel)).await;
    return result;
}

/// Get messages from a queue and write them out to a socket until there are
/// no more messages in the queue or the write breaks for some reason.
async fn connection_write<T: AsyncWrite + Unpin>(
    data: &mut mpsc::Receiver<Bytes>,
    write: &mut T,
) -> Result<(), Error> {
    while let Some(buf) = data.recv().await {
        write.write_all(&buf[..]).await?;
    }
    Ok(())
}

/// Handle a connection, from the socket to the multiplexer and from the
/// multiplexer to the socket.
async fn connection_process(
    channel: u64,
    stream: &mut TcpStream,
    data: &mut mpsc::Receiver<Bytes>,
    writer: &mut mpsc::Sender<Message>,
) {
    let (mut read_half, mut write_half) = stream.split();

    let read = connection_read(channel, &mut read_half, writer);
    let write = connection_write(data, &mut write_half);

    tokio::pin!(read);
    tokio::pin!(write);

    let (mut done_reading, mut done_writing) = (false, false);
    while !(done_reading && done_writing) {
        tokio::select! {
            _ = &mut read, if !done_reading => { done_reading = true; },
            _ = &mut write, if !done_writing => { done_writing = true;},
        }
    }
}

// ----------------------------------------------------------------------------
// Server

struct ServerConnection {
    data: mpsc::Sender<Bytes>,
}

#[derive(Clone)]
struct ServerConnectionTable {
    connections: Arc<Mutex<HashMap<u64, ServerConnection>>>,
}

impl ServerConnectionTable {
    fn new() -> ServerConnectionTable {
        ServerConnectionTable {
            connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn add(self: &mut Self, id: u64, data: mpsc::Sender<Bytes>) {
        let mut connections = self.connections.lock().unwrap();
        connections.insert(id, ServerConnection { data });
    }

    async fn receive(self: &Self, id: u64, buf: Bytes) {
        let data = {
            let connections = self.connections.lock().unwrap();
            if let Some(connection) = connections.get(&id) {
                Some(connection.data.clone())
            } else {
                None
            }
        };

        if let Some(data) = data {
            _ = data.send(buf).await;
        }
    }

    fn remove(self: &mut Self, id: u64) {
        let mut connections = self.connections.lock().unwrap();
        connections.remove(&id);
    }
}

async fn server_handle_connection(
    channel: u64,
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ServerConnectionTable,
) {
    let mut connections = connections;
    if let Ok(mut stream) = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await {
        let (send_data, mut data) = mpsc::channel(32);
        connections.add(channel, send_data);
        if let Ok(_) = writer.send(Message::Connected(channel)).await {
            let mut writer = writer.clone();
            connection_process(channel, &mut stream, &mut data, &mut writer).await;

            eprintln!("< Done server!");
        }
    }

    // Wrong!
    _ = writer.send(Message::Closed(channel));
}

async fn server_read<T: AsyncRead + Unpin>(
    reader: &mut T,
    writer: mpsc::Sender<Message>,
    connections: ServerConnectionTable,
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
                let (writer, connections) = (writer.clone(), connections.clone());
                tokio::spawn(async move {
                    server_handle_connection(channel, port, writer, connections).await;
                });
            }
            Close(channel) => {
                let mut connections = connections.clone();
                tokio::spawn(async move {
                    // Once we get a close the connection becomes  unreachable.
                    //
                    // NOTE: If all goes well the 'data' channel gets dropped
                    // here, and we close the write half of the socket.
                    connections.remove(channel);
                });
            }
            Data(channel, buf) => {
                let connections = connections.clone();
                tokio::spawn(async move {
                    connections.receive(channel, buf).await;
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
    let connections = ServerConnectionTable::new();

    // Jump into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = server_read(reader, msg_sender, connections);
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
    connected: Option<oneshot::Sender<()>>,
    data: mpsc::Sender<Bytes>,
}

struct ClientConnectionTableState {
    next_id: u64,
    connections: HashMap<u64, ClientConnection>,
}

#[derive(Clone)]
struct ClientConnectionTable {
    connections: Arc<Mutex<ClientConnectionTableState>>,
}

impl ClientConnectionTable {
    fn new() -> ClientConnectionTable {
        ClientConnectionTable {
            connections: Arc::new(Mutex::new(ClientConnectionTableState {
                next_id: 0,
                connections: HashMap::new(),
            })),
        }
    }

    fn alloc(self: &mut Self, connected: oneshot::Sender<()>, data: mpsc::Sender<Bytes>) -> u64 {
        let mut tbl = self.connections.lock().unwrap();
        let id = tbl.next_id;
        tbl.next_id += 1;
        tbl.connections.insert(
            id,
            ClientConnection {
                connected: Some(connected),
                data,
            },
        );
        id
    }

    fn connected(self: &mut Self, id: u64) {
        let connected = {
            let mut tbl = self.connections.lock().unwrap();
            if let Some(c) = tbl.connections.get_mut(&id) {
                c.connected.take()
            } else {
                None
            }
        };

        if let Some(connected) = connected {
            _ = connected.send(());
        }
    }

    async fn receive(self: &Self, id: u64, buf: Bytes) {
        let data = {
            let tbl = self.connections.lock().unwrap();
            if let Some(connection) = tbl.connections.get(&id) {
                Some(connection.data.clone())
            } else {
                None
            }
        };

        if let Some(data) = data {
            _ = data.send(buf).await;
        }
    }

    fn remove(self: &mut Self, id: u64) {
        let mut tbl = self.connections.lock().unwrap();
        tbl.connections.remove(&id);
    }
}

async fn client_handle_connection(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ClientConnectionTable,
    socket: &mut TcpStream,
) {
    let mut connections = connections;
    let (send_connected, connected) = oneshot::channel();
    let (send_data, mut data) = mpsc::channel(32);
    let channel = connections.alloc(send_connected, send_data);

    if let Ok(_) = writer.send(Message::Connect(channel, port)).await {
        if let Ok(_) = connected.await {
            let mut writer = writer.clone();
            connection_process(channel, socket, &mut data, &mut writer).await;

            eprintln!("> Done client!");
        } else {
            eprintln!("> Failed to connect to remote");
        }
    }
}

async fn client_listen(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ClientConnectionTable,
) -> Result<(), Error> {
    loop {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
        loop {
            // The second item contains the IP and port of the new
            // connection, but we don't care.
            let (mut socket, _) = listener.accept().await?;

            let (writer, connections) = (writer.clone(), connections.clone());
            tokio::spawn(async move {
                client_handle_connection(port, writer, connections, &mut socket).await;
            });
        }
    }
}

async fn client_read<T: AsyncRead + Unpin>(
    reader: &mut T,
    writer: mpsc::Sender<Message>,
    connections: ClientConnectionTable,
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
            Connected(channel) => {
                let mut connections = connections.clone();
                tokio::spawn(async move {
                    connections.connected(channel);
                });
            }
            Close(channel) => {
                let mut connections = connections.clone();
                tokio::spawn(async move {
                    connections.remove(channel);
                });
            }
            Data(channel, buf) => {
                let connections = connections.clone();
                tokio::spawn(async move {
                    connections.receive(channel, buf).await;
                });
            }
            Ports(ports) => {
                let mut new_listeners = HashMap::new();

                println!("The following ports are available:");
                for port in ports {
                    println!("  {}: {}", port.port, port.desc);

                    let port = port.port;
                    if let Some(l) = listeners.remove(&port) {
                        if !l.is_closed() {
                            // `l` here is, of course, the channel that we
                            // use to tell the listener task to stop (see the
                            // spawn call below). If it isn't closed then
                            // that means a spawn task is still running so we
                            // should just let it keep running and re-use the
                            // existing listener.
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

    let connections = ClientConnectionTable::new();

    // And now really get into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = client_read(reader, msg_sender, connections);
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

/////

pub async fn run_server() {
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
}

pub async fn run_client(remote: &str) {
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
