use anyhow::Result;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io::Cursor;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ----------------------------------------------------------------------------
// Messages

#[derive(Debug, Error)]
pub enum MessageError {
    #[error("Message type unknown: {0}")]
    Unknown(u8),
    #[error("Message incomplete")]
    Incomplete,
}

#[derive(Debug, PartialEq, Clone)]
pub struct PortDesc {
    pub port: u16,
    pub desc: String,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Message {
    Ping,                       // Ignored on both sides, can be used to test connection.
    Hello(u8, u8, Vec<String>), // Server info announcement: major version, minor version, headers.
    Connect(u64, u16),          // Request to connect on a port from client to server.
    Connected(u64),             // Sucessfully connected from server to client.
    Close(u64),                 // Notify that one or the other end of a channel is closed.
    Refresh,                    // Request to refresh list of ports from client.
    Ports(Vec<PortDesc>),       // List of available ports from server to client.
    Data(u64, Bytes),           // Transmit data on a channel.
}

impl Message {
    pub fn encode(self: &Message) -> BytesMut {
        let mut result = BytesMut::new();
        self.encode_buf(&mut result);
        result
    }

    pub fn encode_buf<T: BufMut>(self: &Message, result: &mut T) {
        use Message::*;
        match self {
            Ping => {
                result.put_u8(0x00);
            }
            Hello(major, minor, details) => {
                result.put_u8(0x01);
                result.put_u8(*major);
                result.put_u8(*minor);
                result.put_u16(details.len().try_into().expect("Too many details"));
                for detail in details {
                    put_string(result, detail);
                }
            }
            Connect(channel, port) => {
                result.put_u8(0x02);
                result.put_u64(*channel);
                result.put_u16(*port);
            }
            Connected(channel) => {
                result.put_u8(0x03);
                result.put_u64(*channel);
            }
            Close(channel) => {
                result.put_u8(0x04);
                result.put_u64(*channel);
            }
            Refresh => {
                result.put_u8(0x05);
            }
            Ports(ports) => {
                result.put_u8(0x06);

                result.put_u16(ports.len().try_into().expect("Too many ports"));
                for port in ports {
                    result.put_u16(port.port);

                    // Port descriptions can be long, let's make sure they're not.
                    let sliced = slice_up_to(&port.desc, u16::max_value().into());
                    put_string(result, sliced);
                }
            }
            Data(channel, bytes) => {
                result.put_u8(0x07);
                result.put_u64(*channel);
                result.put_u16(bytes.len().try_into().expect("Payload too big"));
                result.put_slice(bytes); // I hate that this copies. We should make this an async write probably, maybe?
            }
        };
    }

    pub fn decode(cursor: &mut Cursor<&[u8]>) -> Result<Message> {
        use Message::*;
        match get_u8(cursor)? {
            0x00 => Ok(Ping),
            0x01 => {
                let major = get_u8(cursor)?;
                let minor = get_u8(cursor)?;
                let count = get_u16(cursor)?;
                let mut details = Vec::with_capacity(count.into());
                for _ in 0..count {
                    details.push(get_string(cursor)?);
                }
                Ok(Hello(major, minor, details))
            }
            0x02 => {
                let channel = get_u64(cursor)?;
                let port = get_u16(cursor)?;
                Ok(Connect(channel, port))
            }
            0x03 => {
                let channel = get_u64(cursor)?;
                Ok(Connected(channel))
            }
            0x04 => {
                let channel = get_u64(cursor)?;
                Ok(Close(channel))
            }
            0x05 => Ok(Refresh),
            0x06 => {
                let count = get_u16(cursor)?;
                let mut ports = Vec::with_capacity(count.into());
                for _ in 0..count {
                    let port = get_u16(cursor)?;
                    let desc = get_string(cursor)?;
                    ports.push(PortDesc { port, desc });
                }
                Ok(Ports(ports))
            }
            0x07 => {
                let channel = get_u64(cursor)?;
                let length = get_u16(cursor)?;
                let data = get_bytes(cursor, length.into())?;
                Ok(Data(channel, data))
            }
            b => Err(MessageError::Unknown(b).into()),
        }
    }
}

fn get_u8(cursor: &mut Cursor<&[u8]>) -> Result<u8, MessageError> {
    if !cursor.has_remaining() {
        return Err(MessageError::Incomplete);
    }
    Ok(cursor.get_u8())
}

fn get_u16(cursor: &mut Cursor<&[u8]>) -> Result<u16, MessageError> {
    if cursor.remaining() < 2 {
        return Err(MessageError::Incomplete);
    }
    Ok(cursor.get_u16())
}

fn get_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64, MessageError> {
    if cursor.remaining() < 8 {
        return Err(MessageError::Incomplete);
    }
    Ok(cursor.get_u64())
}

fn get_bytes(cursor: &mut Cursor<&[u8]>, length: usize) -> Result<Bytes, MessageError> {
    if cursor.remaining() < length {
        return Err(MessageError::Incomplete);
    }

    Ok(cursor.copy_to_bytes(length))
}

fn get_string(cursor: &mut Cursor<&[u8]>) -> Result<String> {
    let length = get_u16(cursor)?;

    let data = get_bytes(cursor, length.into())?;
    Ok(std::str::from_utf8(&data[..])?.to_owned())
}

fn slice_up_to(s: &str, max_len: usize) -> &str {
    if max_len >= s.len() {
        return s;
    }
    let mut idx = max_len;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

fn put_string<T: BufMut>(target: &mut T, str: &str) {
    target.put_u16(str.len().try_into().expect("String is too long"));
    target.put_slice(str.as_bytes());
}

// ----------------------------------------------------------------------------
// Message IO

pub struct MessageWriter<T: AsyncWrite + Unpin> {
    writer: T,
}

impl<T: AsyncWrite + Unpin> MessageWriter<T> {
    pub fn new(writer: T) -> MessageWriter<T> {
        MessageWriter { writer }
    }
    pub async fn write(self: &mut Self, msg: Message) -> Result<()> {
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

pub struct MessageReader<T: AsyncRead + Unpin> {
    reader: T,
}

impl<T: AsyncRead + Unpin> MessageReader<T> {
    pub fn new(reader: T) -> MessageReader<T> {
        MessageReader { reader }
    }
    pub async fn read(self: &mut Self) -> Result<Message> {
        let frame_length = self.reader.read_u32().await?;
        let mut data = BytesMut::with_capacity(frame_length.try_into().unwrap());
        self.reader.read_buf(&mut data).await?;

        let mut cursor = Cursor::new(&data[..]);
        Message::decode(&mut cursor)
    }
}

#[cfg(test)]
mod message_tests {
    use crate::message::Message::*;
    use crate::message::PortDesc;
    use crate::message::{Message, MessageReader, MessageWriter};

    fn assert_round_trip(message: Message) {
        let encoded = message.encode();
        let mut cursor = std::io::Cursor::new(&encoded[..]);
        let result = Message::decode(&mut cursor).unwrap();
        assert_eq!(message.clone(), result);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Unable to start tokio runtime");

        rt.block_on(async move {
            let (client, server) = tokio::io::duplex(64);

            let expected = message.clone();
            let write = tokio::spawn(async move {
                let mut writer = MessageWriter::new(client);
                writer.write(message).await.expect("Write failed");
            });

            let read = tokio::spawn(async move {
                let mut reader = MessageReader::new(server);
                let actual = reader.read().await.expect("Read failed");
                assert_eq!(expected, actual);
            });

            write.await.expect("Write proc failed");
            read.await.expect("Read proc failed");
        });
    }

    #[test]
    fn round_trip() {
        assert_round_trip(Ping);
        assert_round_trip(Hello(
            0x12,
            0x00,
            vec!["One".to_string(), "Two".to_string(), "Three".to_string()],
        ));
        assert_round_trip(Hello(0x00, 0x01, vec![]));
        assert_round_trip(Connect(0x1234567890123456, 0x1234));
        assert_round_trip(Connected(0x1234567890123456));
        assert_round_trip(Close(0x1234567890123456));
        assert_round_trip(Refresh);
        assert_round_trip(Ports(vec![]));
        assert_round_trip(Ports(vec![
            PortDesc {
                port: 8080,
                desc: "query-service".to_string(),
            },
            PortDesc {
                port: 9090,
                desc: "metadata-library".to_string(),
            },
        ]));
        assert_round_trip(Data(0x1234567890123456, vec![1, 2, 3, 4].into()));
    }

    #[test]
    fn big_port_desc() {
        // Strings are capped at 64k let's make a big one!
        let char = String::from_utf8(vec![0xe0, 0xa0, 0x83]).unwrap();
        let mut str = String::with_capacity(128 * 1024);
        while str.len() < 128 * 1024 {
            str.push_str(&char);
        }

        let msg = Ports(vec![PortDesc {
            port: 8080,
            desc: str,
        }]);
        msg.encode();
    }
}
