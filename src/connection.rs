use crate::message::Message;
use crate::Error;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

const MAX_PACKET: usize = u16::max_value() as usize;

/// Read from a socket and convert the reads into Messages to put into the
/// queue until the socket is closed for reading or an error occurs. We read
/// at most 2^16-1 bytes at a time, accepting the overhead of multiple reads
/// to keep one writer from clogging the pipe for everybody else. Each read
/// is converted into a [Message::Data] message that is sent to the `writer`
/// channel.
///
/// Once we're done reading (either because of a connection error or a clean
/// shutdown) we send a [Message::Close] message on the channel before
/// returning.
///
/// # Errors
/// If an error occurs reading from `read` we return [Error::IO]. If the
/// message channel is closed before we can send to it then we return
/// [Error::ConnectionReset].
///
async fn connection_read<T: AsyncRead + Unpin>(
    channel: u64,
    read: &mut T,
    writer: &mut mpsc::Sender<Message>,
) -> Result<(), Error> {
    let result = loop {
        let mut buffer = BytesMut::with_capacity(MAX_PACKET);
        if let Err(e) = read.read_buf(&mut buffer).await {
            break Err(Error::IO(e));
        }
        if buffer.len() == 0 {
            break Ok(());
        }

        if let Err(_) = writer.send(Message::Data(channel, buffer.into())).await {
            break Err(Error::ConnectionReset);
        }

        // TODO: Flow control here, wait for the packet to be acknowleged so
        // there isn't head-of-line blocking or infinite buffering on the
        // remote side. Also buffer re-use!
    };

    // We are effectively closed on this side, send the close to drop the
    // corresponding write side on the other end of the pipe.
    _ = writer.send(Message::Close(channel)).await;
    return result;
}

/// Get messages from a queue and write them out to a socket until there are
/// no more messages in the queue or a write fails for some reason.
///
/// # Errors
/// If a write fails this returns `Error::IO`.
///
async fn connection_write<T: AsyncWrite + Unpin>(
    data: &mut mpsc::Receiver<Bytes>,
    write: &mut T,
) -> Result<(), Error> {
    while let Some(buf) = data.recv().await {
        if let Err(e) = write.write_all(&buf[..]).await {
            return Err(Error::IO(e));
        }
    }
    Ok(())
}

/// Handle a connection, from the socket to the multiplexer and from the
/// multiplexer to the socket. Keeps running until both the read and write
/// side are closed. In natural circumstances, we expect the write side to
/// close when the `data` sender is dropped from the connection table (see
/// `ConnectionTable`), and we expect the read side to close when the
/// socket's read half closes (which will cause a `Close` to be sent which
/// should drop the `data` sender on the other side, etc...).
///
pub async fn process(
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

/// The connection structure tracks the various channels used to communicate
/// with an "open" connection.
struct Connection {
    /// The callback for the connected message, if we haven't already
    /// connected across the channel. Realistically, this only ever has a
    /// value on the client side, where we wait for the server side to
    /// connect and then acknowlege that the connection.
    connected: Option<oneshot::Sender<()>>,

    /// The channel where the connection receives [Bytes] to be written to
    /// the socket.
    data: mpsc::Sender<Bytes>,
}

struct ConnectionTableState {
    next_id: u64,
    connections: HashMap<u64, Connection>,
}

/// A tracking structure for connections. This structure is thread-safe and
/// so can be used to track new connections from as many concurrent listeners
/// as you would like.
#[derive(Clone)]
pub struct ConnectionTable {
    connections: Arc<Mutex<ConnectionTableState>>,
}

impl ConnectionTable {
    /// Create a new, empty connection table.
    pub fn new() -> ConnectionTable {
        ConnectionTable {
            connections: Arc::new(Mutex::new(ConnectionTableState {
                next_id: 0,
                connections: HashMap::new(),
            })),
        }
    }

    /// Allocate a new connection on the client side. The connection is
    /// assigned a new ID, which is returned to the caller.
    pub fn alloc(
        self: &mut Self,
        connected: oneshot::Sender<()>,
        data: mpsc::Sender<Bytes>,
    ) -> u64 {
        let mut tbl = self.connections.lock().unwrap();
        let id = tbl.next_id;
        tbl.next_id += 1;
        tbl.connections.insert(
            id,
            Connection {
                connected: Some(connected),
                data,
            },
        );
        id
    }

    /// Add a connection to the table on the server side. The client sent us
    /// the ID to use, so we don't need to allocate it, and obviously we
    /// aren't going to be waiting for the connection to be "connected."
    pub fn add(self: &mut Self, id: u64, data: mpsc::Sender<Bytes>) {
        let mut tbl = self.connections.lock().unwrap();
        tbl.connections.insert(
            id,
            Connection {
                connected: None,
                data,
            },
        );
    }

    /// Mark a connection as being "connected", on the client side, where we
    /// wait for the server to tell us such things. Note that this gets used
    /// for a successful connection; on a failure just call [remove].
    pub fn connected(self: &mut Self, id: u64) {
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

    /// Tell a connection that we have received data. This gets used on both
    /// sides of the pipe; if the connection exists and is still active it
    /// will send the data out through its socket.
    pub async fn receive(self: &Self, id: u64, buf: Bytes) {
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

    /// Remove a connection from the table, effectively closing it. This will
    /// close all the pipes that the connection uses to receive data from the
    /// other side, performing a cleanup on our "write" side of the socket.
    pub fn remove(self: &mut Self, id: u64) {
        let mut tbl = self.connections.lock().unwrap();
        tbl.connections.remove(&id);
    }
}
