use bytes::{Buf, BufMut, Bytes, BytesMut};
use procfs::process::FDTarget;
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, ReadHalf, WriteHalf,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

// =============================================================
// Looking for listening ports
// =============================================================

struct PortDesc {
    port: u16,
    desc: String,
}

fn get_entries() -> procfs::ProcResult<Vec<PortDesc>> {
    let all_procs = procfs::process::all_processes()?;

    // build up a map between socket inodes and process stat info. Ignore any
    // error we encounter as it probably means we have no access to that
    // process or something.
    let mut map: HashMap<u64, String> = HashMap::new();
    for p in all_procs {
        if let Ok(process) = p {
            if !process.is_alive() {
                continue; // Ignore zombies.
            }

            if let (Ok(fds), Ok(cmd)) = (process.fd(), process.cmdline()) {
                for fd in fds {
                    if let Ok(fd) = fd {
                        if let FDTarget::Socket(inode) = fd.target {
                            map.insert(inode, cmd.join(" "));
                        }
                    }
                }
            }
        }
    }

    let mut h: HashMap<u16, PortDesc> = HashMap::new();

    // Go through all the listening IPv4 and IPv6 sockets and take the first
    // instance of listening on each port *if* the address is loopback or
    // unspecified. (TODO: Do we want this restriction really?)
    let tcp = procfs::net::tcp()?;
    let tcp6 = procfs::net::tcp6()?;
    for tcp_entry in tcp.into_iter().chain(tcp6) {
        if tcp_entry.state == procfs::net::TcpState::Listen
            && (tcp_entry.local_address.ip().is_loopback()
                || tcp_entry.local_address.ip().is_unspecified())
            && !h.contains_key(&tcp_entry.local_address.port())
        {
            if let Some(cmd) = map.get(&tcp_entry.inode) {
                h.insert(
                    tcp_entry.local_address.port(),
                    PortDesc {
                        port: tcp_entry.local_address.port(),
                        desc: cmd.clone(),
                    },
                );
            }
        }
    }

    Ok(h.into_values().collect())
}

// =============================================================
// Sending and receiving data
// =============================================================

// A channel that can receive packets from the remote side.
struct Channel {
    packets: mpsc::Sender<IncomingPacket>, // TODO: spsc probably
}

struct Channels {
    channels: Mutex<HashMap<u16, Arc<Channel>>>,
    removed: mpsc::Sender<u16>, // TODO: send an error?
}

impl Channels {
    fn new(removed: mpsc::Sender<u16>) -> Channels {
        Channels {
            channels: Mutex::new(HashMap::new()),
            removed: removed,
        }
    }

    fn add(self: &Self, id: u16, channel: Channel) {
        let mut channels = self.channels.lock().unwrap();
        channels.insert(id, Arc::new(channel));
    }

    fn get(self: &Self, id: u16) -> Option<Arc<Channel>> {
        let channels = self.channels.lock().unwrap();
        if let Some(channel) = channels.get(&id) {
            Some(channel.clone())
        } else {
            None
        }
    }

    async fn remove(self: &Self, id: u16) {
        {
            let mut channels = self.channels.lock().unwrap();
            channels.remove(&id);
        }
        _ = self.removed.send(id).await;
    }
}

enum Error {
    Incomplete,
    UnknownMessage,
    Corrupt,
}

enum Message {
    Ping,
    Connect(u64, u16),    // Request to connect on a port from client to server.
    Connected(u64, u16),  // Sucessfully connected from server to client.
    Close(u64),           // Request to close from client to server.
    Abort(u64),           // Notify of close from server to client.
    Closed(u64),          // Response to Close or Abort.
    Refresh,              // Request to refresh list of ports from client.
    Ports(Vec<PortDesc>), // List of available ports from server to client.
}

impl Message {
    fn encode(self: &Message, dest: T) -> BytesMut {
        use Message::*;
        let result = BytesMut::new();
        match self {
            Ping => {
                result.put_u8(0x00);
            }
            Connect(channel, port) => {
                result.put_u8(0x01);
                result.put_u64(*channel);
                result.put_u16(*port);
            }
            Connected(channel, port) => {
                result.put_u8(0x02);
                result.put_u64(*channel);
                result.put_u16(*port);
            }
            Close(channel) => {
                result.put_u8(0x03);
                result.put_u64(*channel);
            }
            Abort(channel) => {
                result.put_u8(0x04);
                result.put_u64(*channel);
            }
            Closed(channel) => {
                result.put_u8(0x05);
                result.put_u64(*channel);
            }
            Refresh => {
                result.put_u8(0x06);
            }
            Ports(ports) => {
                result.put_u8(0x07);

                result.put_u16(u16::try_from(ports.len()).expect("Too many ports"));
                for port in ports {
                    result.put_u16(port.port);

                    let sliced = slice_up_to(&port.desc, u16::max_value().into());
                    result.put_u16(u16::try_from(sliced.len()).unwrap());
                    result.put_slice(sliced.as_bytes());
                }
            }
        };
        result
    }

    fn decode(cursor: &mut Cursor<&[u8]>) -> Result<Message, Error> {
        use Message::*;
        match get_u8(cursor)? {
            0x00 => Ok(Ping),
            0x01 => {
                let channel = get_u64(cursor)?;
                let port = get_u16(cursor)?;
                Ok(Connect(channel, port))
            }
            0x02 => {
                let channel = get_u64(cursor)?;
                let port = get_u16(cursor)?;
                Ok(Connected(channel, port))
            }
            0x03 => {
                let channel = get_u64(cursor)?;
                Ok(Close(channel))
            }
            0x04 => {
                let channel = get_u64(cursor)?;
                Ok(Abort(channel))
            }
            0x05 => {
                let channel = get_u64(cursor)?;
                Ok(Closed(channel))
            }
            0x06 => Ok(Refresh),
            0x07 => {
                let count = get_u16(cursor)?;

                let mut ports = Vec::new();
                for i in 0..count {
                    let port = get_u16(cursor)?;
                    let length = get_u16(cursor)?;

                    let data = get_bytes(cursor, length.into())?;
                    let desc = match std::str::from_utf8(&data[..]) {
                        Ok(s) => s.to_owned(),
                        Err(_) => return Err(Error::Corrupt),
                    };

                    ports.push(PortDesc { port, desc });
                }
                Ok(Ports(ports))
            }
            _ => Err(Error::Corrupt),
        }
    }
}

fn get_u8(cursor: &mut Cursor<&[u8]>) -> Result<u8, Error> {
    if !cursor.has_remaining() {
        return Err(Error::Incomplete);
    }
    Ok(cursor.get_u8())
}

fn get_u16(cursor: &mut Cursor<&[u8]>) -> Result<u16, Error> {
    if cursor.remaining() < 2 {
        return Err(Error::Incomplete);
    }
    Ok(cursor.get_u16())
}

fn get_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64, Error> {
    if cursor.remaining() < 8 {
        return Err(Error::Incomplete);
    }
    Ok(cursor.get_u64())
}

fn get_bytes(cursor: &mut Cursor<&[u8]>, length: usize) -> Result<Bytes, Error> {
    if cursor.remaining() < length {
        return Err(Error::Incomplete);
    }

    Ok(cursor.copy_to_bytes(length))
}

pub fn slice_up_to(s: &str, max_len: usize) -> &str {
    if max_len >= s.len() {
        return s;
    }
    let mut idx = max_len;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

struct ControllerState {
    on_removed: mpsc::Receiver<u16>,
    on_packet: mpsc::Receiver<IncomingPacket>,
}

struct ClientController {
    channels: Arc<Channels>,
    state: Option<ControllerState>,
}

impl ClientController {
    fn new() -> ClientController {
        let (packets, on_packet) = mpsc::channel(32);
        let (removed, on_removed) = mpsc::channel(32);

        let channels = Arc::new(Channels::new(removed));
        channels.add(0, Channel { packets });

        ClientController {
            channels,
            state: Some(ControllerState {
                on_removed,
                on_packet,
            }),
        }
    }

    fn channels(self: &Self) -> Arc<Channels> {
        self.channels.clone()
    }

    fn start(self: &mut Self, stop: oneshot::Receiver<()>) {
        if let Some(state) = self.state.take() {
            tokio::spawn(async move {
                let mut state = state;
                tokio::select! {
                    _ = Self::process_channel_remove(&mut state.on_removed) => (),
                    _ = Self::process_packets(&mut state.on_packet) => (),
                    _ = stop => (),
                }
            });
        }
    }

    async fn process_packets(packets: &mut mpsc::Receiver<IncomingPacket>) {
        while let Some(_) = packets.recv().await {}
    }

    async fn process_channel_remove(removals: &mut mpsc::Receiver<u16>) {
        while let Some(_) = removals.recv().await {}
    }
}

// TODO: Need flow control on the send side too because we don't want to
//       block everybody if there's a slow reader on the other side. So the
//       completion one-shot we send to the mux needs to go in a table until
//       we get an ack back across the TCP channel.

// A packet being sent across the channel.
struct OutgoingPacket {
    channel: u16, // Channel 0 reserved as control channel.
    data: BytesMut,
    // Where we notify folks when the data has been sent.
    sent: oneshot::Sender<BytesMut>,
}

// This is the "read" side of a forwarded connection; it reads from the read
// half of a TCP stream and sends those reads to into the multiplexing
// connection to be sent to the other side.
//
// (There's another function that handles the write side of a connection.)
async fn handle_read_side(
    channel: u16,
    read: &mut ReadHalf<TcpStream>,
    mux: mpsc::Sender<OutgoingPacket>,
) -> Result<(), tokio::io::Error> {
    let mut buffer = BytesMut::with_capacity(u16::max_value().into());
    loop {
        read.read_buf(&mut buffer).await?;

        let (tx, rx) = oneshot::channel::<BytesMut>();
        let op = OutgoingPacket {
            channel,
            data: buffer,
            sent: tx,
        };
        if let Err(_) = mux.send(op).await {
            return Ok(());
        }

        match rx.await {
            Ok(b) => buffer = b,
            Err(_) => return Ok(()),
        }

        assert_eq!(buffer.capacity(), u16::max_value().into());
        buffer.clear();
    }
}

struct IncomingPacket {
    data: BytesMut,
    sent: oneshot::Sender<BytesMut>,
}

// This is the "write" side of a forwarded connection; it receives data from
// the multiplexed connection and sends it out over the write half of a TCP
// stream to the attached process.
//
// (There's another function that handles the "read" side of a connection,
// which is a little more complex.)
async fn handle_write_side(
    channel: u16,
    packets: &mut mpsc::Receiver<IncomingPacket>, // TODO: spsc I think
    write: &mut WriteHalf<TcpStream>,
) -> Result<(), tokio::io::Error> {
    while let Some(IncomingPacket { data, sent }) = packets.recv().await {
        // Write the data out to the write end of the TCP stream.
        write.write_all(&data[..]).await?;

        // Now we've sent it, we can send the buffer back and let the caller
        // know we wrote it. Literally don't care if they're still listening
        // or not; should I care?
        _ = sent.send(data);
    }

    Ok(())
}

async fn handle_connection(
    packets: &mut mpsc::Receiver<IncomingPacket>, // TODO: spsc I think
    mux: mpsc::Sender<OutgoingPacket>,
    channel: u16,
    socket: TcpStream,
) -> Result<(), tokio::io::Error> {
    // Handle the read and write side of the socket separately.
    let (mut read, mut write) = tokio::io::split(socket);

    let writer = handle_write_side(channel, packets, &mut write);
    let reader = handle_read_side(channel, &mut read, mux);

    // Wait for both to be done. If either the reader or the writer completes
    // with an error then we're just going to shut down the whole thing,
    // closing the socket, &c. But either side can shut down cleanly and
    // that's fine!
    tokio::pin!(writer);
    tokio::pin!(reader);
    let (mut read_done, mut write_done) = (false, false);
    while !(read_done && write_done) {
        tokio::select! {
            write_result = &mut writer, if !write_done => {
                write_done = true;
                if let Err(e) = write_result {
                    return Err(e);
                }
            },
            read_result = &mut reader, if !read_done => {
                read_done = true;
                if let Err(e) = read_result {
                    return Err(e);
                }
            },
        }
    }

    Ok(())
}

async fn allocate_channel() -> (
    u16,
    mpsc::Receiver<IncomingPacket>,
    mpsc::Sender<OutgoingPacket>,
) {
    panic!("Not implemented");
}

// This is only on the client side of the connection.
async fn handle_listen(port: u16, channels: Arc<Channels>) -> Result<(), tokio::io::Error> {
    loop {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
        loop {
            // The second item contains the IP and port of the new connection.
            // TODO: Handle shutdown correctly.
            let (socket, _) = listener.accept().await?;

            tokio::spawn(async move {
                // Finish the connect and then....
                let (channel, mut packets, mux) = allocate_channel().await;

                // ....handle the connection asynchronously...
                let result = handle_connection(&mut packets, mux, channel, socket).await;

                // ...and then shut it down.
                // close_channel(channel, result).await;
            });
        }
    }
}

// Multiplex writes onto the given writer. Writes come in through the
// specified channel, and are multiplexed to the writer.
async fn mux_packets<T: AsyncWrite + Unpin>(
    rx: &mut mpsc::Receiver<OutgoingPacket>,
    writer: &mut T,
) {
    while let Some(OutgoingPacket {
        channel,
        data,
        sent,
    }) = rx.recv().await
    {
        // Send the packet over the shared connection.
        // TODO: Technically, for flow control purposes, we should mark
        //       the transmission as pending right now, and wait for an ack
        //       from the server in order to "complete" the write. OR we
        //       should do even better and caqp the number of outstanding
        //       writes per channel.
        writer
            .write_u16(channel)
            .await
            .expect("Error writing channel");

        writer
            .write_u16(u16::try_from(data.len()).expect("Multiplexed buffer too big"))
            .await
            .expect("Error writing length");

        writer
            .write_all(&data[..])
            .await
            .expect("Error writing data");

        sent.send(data).expect("Error notifying of completion");
    }
}

fn new_muxer<T: AsyncWrite + Unpin + Send + 'static>(writer: T) -> mpsc::Sender<OutgoingPacket> {
    let mut writer = writer;
    let (tx, mut rx) = mpsc::channel::<OutgoingPacket>(32);
    tokio::spawn(async move {
        mux_packets(&mut rx, &mut writer).await;
    });
    tx
}

async fn demux_packets<T: AsyncRead + Unpin>(reader: &mut T, channels: Arc<Channels>) {
    let mut buffer = BytesMut::with_capacity(u16::max_value().into());
    loop {
        let chid = reader
            .read_u16()
            .await
            .expect("Error reading channel number from connection");
        let length = reader
            .read_u16()
            .await
            .expect("Error reading length from connection");

        let tail = buffer.split_off(length.into());
        reader
            .read_exact(&mut buffer)
            .await
            .expect("Error reading data from connection");

        if let Some(channel) = channels.get(chid) {
            let (sent, is_sent) = oneshot::channel::<BytesMut>();
            let packet = IncomingPacket { data: buffer, sent };

            if let Err(_) = channel.packets.send(packet).await {
                // TODO: Log Error
                buffer = BytesMut::with_capacity(u16::max_value().into());
                channels.remove(chid).await;
            } else {
                match is_sent.await {
                    Ok(b) => {
                        buffer = b;
                        buffer.unsplit(tail);
                    }
                    Err(_) => {
                        // TODO: Log Error
                        buffer = BytesMut::with_capacity(u16::max_value().into());
                        channels.remove(chid).await;
                    }
                }
            }
        }

        buffer.clear();
    }
}

async fn spawn_ssh(server: String) -> Result<tokio::process::Child, tokio::io::Error> {
    // let mut cmd = process::Command::new("echo");
    // cmd.stdout(Stdio::piped());
    // cmd.stdin(Stdio::piped());
    panic!("Not Implemented");
}

#[tokio::main]
async fn main() {
    // Create the client-side controller.
    let mut controller = ClientController::new();

    // Spawn an SSH connection to the remote side.
    let mut child = spawn_ssh("coder.doty-dev".into())
        .await
        .expect("failed to spawn");

    // Build a multiplexer around stdin.
    let muxer = new_muxer(BufWriter::new(
        child
            .stdin
            .take()
            .expect("child did not have a handle to stdin"),
    ));

    // Buffer input and output, FOR SPEED!
    let mut reader = BufReader::new(
        child
            .stdout
            .take()
            .expect("child did not have a handle to stdout"),
    );

    let channels = controller.channels().clone();
    tokio::spawn(async move {
        demux_packets(&mut reader, channels).await;
    });

    //    let mut writer =
    // Start up a task that's watching the SSH connection for completion.
    // Presumably stdin and stdout will be closed and I'll get read/write
    // errors and whatnot.
    tokio::spawn(async move {
        let status = child
            .wait()
            .await
            .expect("child process encountered an error");

        println!("child status was: {}", status);
    });

    // TODO: Wait for stdout to indicate readiness, or for a timeout to indicate it
    //       hasn't started.  Note that some ssh implementations spit stuff
    //       into stdout that we ought to ignore, or there's a login MOTD or
    //       something, and we should just ignore it until we see the magic
    //       bytes.

    let (send_stop, stop) = oneshot::channel();
    controller.start(stop);

    // I guess we stop on a control-C?

    _ = send_stop.send(());
}
