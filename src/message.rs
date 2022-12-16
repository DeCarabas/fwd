use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io::Cursor;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ----------------------------------------------------------------------------
// Errors

#[derive(Debug, Error)]
pub enum Error {
    #[error("Message type unknown: {0}")]
    Unknown(u8),
    #[error("Message incomplete")]
    Incomplete,
    #[error("String contained invalid UTF8: {0}")]
    InvalidString(std::str::Utf8Error),
    #[error("IO Error occurred: {0}")]
    IO(std::io::Error),
}

impl From<std::str::Utf8Error> for Error {
    fn from(value: std::str::Utf8Error) -> Self {
        Self::InvalidString(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::IO(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ----------------------------------------------------------------------------
// Messages

#[derive(Debug, PartialEq, Clone)]
pub struct PortDesc {
    pub port: u16,
    pub desc: String,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Message {
    // Ignored on both sides, can be used to test connection.
    Ping,

    // Server info announcement: major version, minor version, headers.
    Hello(u8, u8, Vec<String>),

    // Request to refresh list of ports from client.
    Refresh,

    // List of available ports from server to client.
    Ports(Vec<PortDesc>),
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
                result.put_u16(
                    details.len().try_into().expect("Too many details"),
                );
                for detail in details {
                    put_string(result, detail);
                }
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
                    let sliced =
                        slice_up_to(&port.desc, u16::max_value().into());
                    put_string(result, sliced);
                }
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
            b => Err(Error::Unknown(b).into()),
        }
    }
}

fn get_u8(cursor: &mut Cursor<&[u8]>) -> Result<u8> {
    if !cursor.has_remaining() {
        return Err(Error::Incomplete);
    }
    Ok(cursor.get_u8())
}

fn get_u16(cursor: &mut Cursor<&[u8]>) -> Result<u16> {
    if cursor.remaining() < 2 {
        return Err(Error::Incomplete);
    }
    Ok(cursor.get_u16())
}

fn get_bytes(cursor: &mut Cursor<&[u8]>, length: usize) -> Result<Bytes> {
    if cursor.remaining() < length {
        return Err(Error::Incomplete);
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
        self.writer.write_all(&mut buffer).await?;
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
        let mut data = vec![0; frame_length.try_into().unwrap()];
        self.reader.read_exact(&mut data).await?;

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
    }

    #[test]
    fn big_port_desc() {
        // Strings are capped at 64k let's make a big one!
        let char = String::from_utf8(vec![0xe0, 0xa0, 0x83]).unwrap();
        let mut str = String::with_capacity(128 * 1024);
        while str.len() < 128 * 1024 {
            str.push_str(&char);
        }

        let msg = Ports(vec![PortDesc { port: 8080, desc: str }]);
        msg.encode();
    }
}
