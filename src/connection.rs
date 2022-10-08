use crate::message::Message;
use crate::Error;
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

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
            break Err(Error::IO(e));
        }
        if buffer.len() == 0 {
            break Ok(());
        }

        if let Err(_) = writer.send(Message::Data(channel, buffer.into())).await {
            break Err(Error::ConnectionReset);
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
        if let Err(e) = write.write_all(&buf[..]).await {
            return Err(Error::IO(e));
        }
    }
    Ok(())
}

/// Handle a connection, from the socket to the multiplexer and from the
/// multiplexer to the socket.
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
