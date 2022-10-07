use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::io::Cursor;

#[derive(Debug, PartialEq)]
pub enum MessageError {
    Incomplete,
    UnknownMessage,
    Corrupt,
}

#[derive(Debug, PartialEq, Clone)]
pub struct PortDesc {
    pub port: u16,
    pub desc: String,
}

#[derive(Debug, PartialEq)]
pub enum Message {
    Ping,
    Connect(u64, u16), // Request to connect on a port from client to server.
    Connected(u64),    // Sucessfully connected from server to client.
    Close(u64),        // Request to close connection on either end.
    // Abort(u64),        // Notify of close from server to client.
    Closed(u64),          // Response to Close or Abort.
    Refresh,              // Request to refresh list of ports from client.
    Ports(Vec<PortDesc>), // List of available ports from server to client.
    Data(u64, Bytes),     // Transmit data.
}

impl Message {
    pub fn encode(self: &Message) -> BytesMut {
        use Message::*;
        let mut result = BytesMut::new();
        match self {
            Ping => {
                result.put_u8(0x00);
            }
            Connect(channel, port) => {
                result.put_u8(0x01);
                result.put_u64(*channel);
                result.put_u16(*port);
            }
            Connected(channel) => {
                result.put_u8(0x02);
                result.put_u64(*channel);
            }
            Close(channel) => {
                result.put_u8(0x03);
                result.put_u64(*channel);
            }
            // Abort(channel) => {
            //     result.put_u8(0x04);
            //     result.put_u64(*channel);
            // }
            Closed(channel) => {
                result.put_u8(0x05);
                result.put_u64(*channel);
            }
            Refresh => {
                result.put_u8(0x06);
            }
            Ports(ports) => {
                result.put_u8(0x07);

                result.put_u16(ports.len().try_into().expect("Too many ports"));
                for port in ports {
                    result.put_u16(port.port);

                    let sliced = slice_up_to(&port.desc, u16::max_value().into());
                    result.put_u16(sliced.len().try_into().unwrap());
                    result.put_slice(sliced.as_bytes());
                }
            }
            Data(channel, bytes) => {
                result.put_u8(0x08);
                result.put_u64(*channel);
                result.put_u16(bytes.len().try_into().expect("Payload too big"));
                result.put_slice(bytes); // I hate that this copies. We should make this an async write probably.
            }
        };
        result
    }

    pub fn decode(cursor: &mut Cursor<&[u8]>) -> Result<Message, MessageError> {
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
                Ok(Connected(channel))
            }
            0x03 => {
                let channel = get_u64(cursor)?;
                Ok(Close(channel))
            }
            // 0x04 => {
            //     let channel = get_u64(cursor)?;
            //     Ok(Abort(channel))
            // }
            0x05 => {
                let channel = get_u64(cursor)?;
                Ok(Closed(channel))
            }
            0x06 => Ok(Refresh),
            0x07 => {
                let count = get_u16(cursor)?;

                let mut ports = Vec::new();
                for _ in 0..count {
                    let port = get_u16(cursor)?;
                    let length = get_u16(cursor)?;

                    let data = get_bytes(cursor, length.into())?;
                    let desc = match std::str::from_utf8(&data[..]) {
                        Ok(s) => s.to_owned(),
                        Err(_) => return Err(MessageError::Corrupt),
                    };

                    ports.push(PortDesc { port, desc });
                }
                Ok(Ports(ports))
            }
            0x08 => {
                let channel = get_u64(cursor)?;
                let length = get_u16(cursor)?;
                let data = get_bytes(cursor, length.into())?;
                Ok(Data(channel, data))
            }
            _ => Err(MessageError::UnknownMessage),
        }
    }
}

#[cfg(test)]
mod message_tests {
    use crate::message::Message;
    use crate::message::Message::*;
    use crate::message::PortDesc;

    fn assert_round_trip(message: Message) {
        let encoded = message.encode();
        let mut cursor = std::io::Cursor::new(&encoded[..]);
        let result = Message::decode(&mut cursor);
        assert_eq!(Ok(message), result);
    }

    #[test]
    fn round_trip() {
        assert_round_trip(Ping);
        assert_round_trip(Connect(0x1234567890123456, 0x1234));
        assert_round_trip(Connected(0x1234567890123456));
        assert_round_trip(Close(0x1234567890123456));
        // assert_round_trip(Abort(0x1234567890123456));
        assert_round_trip(Closed(0x1234567890123456));
        assert_round_trip(Refresh);
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
