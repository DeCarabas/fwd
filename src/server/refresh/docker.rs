use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

use crate::message::PortDesc;

pub const DEFAULT_DOCKER_HOST: &str = "unix:///var/run/docker.sock";

async fn list_containers_with_connection<T>(stream: T) -> Result<Vec<u8>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    // Send this one exact request. (Who needs an HTTP library?)
    const DOCKER_LIST_CONTAINERS: &[u8] = b"\
GET /containers/json HTTP/1.1\r\n\
Host: localhost\r\n\
User-Agent: fwd/1.0\r\n\
Accept: */*\r\n\
\r\n";
    let mut stream = tokio::io::BufStream::new(stream);
    stream.write_all(DOCKER_LIST_CONTAINERS).await?;
    stream.flush().await?;

    // Check the HTTP response.
    let mut line = String::new();
    stream.read_line(&mut line).await?;
    let parts: Vec<&str> = line.split(" ").collect();
    if parts.len() < 2 || parts[1] != "200" {
        bail!("Error response from docker: {line}");
    }

    // Process the headers; all we really care about is content-length.
    let mut content_length: usize = 0;
    loop {
        line.clear();
        stream.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
        line.make_ascii_lowercase();
        if let Some(rest) = line.strip_prefix("content-length: ") {
            content_length = rest.trim().parse()?;
        }
    }

    // Read the JSON response.
    let mut response_buffer = vec![0; content_length];
    stream.read_exact(&mut response_buffer).await?;

    // Done with the stream.
    Ok(response_buffer)
}

async fn list_containers() -> Result<Vec<u8>> {
    let host = std::env::var("DOCKER_HOST")
        .unwrap_or_else(|_| DEFAULT_DOCKER_HOST.to_string());
    match host {
        h if h.starts_with("unix://") => {
            let socket_path = &h[7..];
            let socket = tokio::net::UnixStream::connect(socket_path).await?;
            list_containers_with_connection(socket).await
        }
        h if h.starts_with("tcp://") => {
            let host_port = &h[6..]; // TODO: Routing to sub-paths?
            let socket = tokio::net::TcpStream::connect(host_port).await?;
            list_containers_with_connection(socket).await
        }
        h if h.starts_with("http://") => {
            let host_port = &h[7..]; // TODO: Routing to sub-paths?
            let socket = tokio::net::TcpStream::connect(host_port).await?;
            list_containers_with_connection(socket).await
        }
        _ => bail!("Unsupported docker host: {host}"),
    }
}

#[derive(Debug, PartialEq)]
enum JsonValue {
    Null,
    True,
    False,
    Number(f64),
    String(String),
    Object(HashMap<String, JsonValue>),
    Array(Vec<JsonValue>),
}

impl JsonValue {
    pub fn parse(blob: &[u8]) -> Result<Self> {
        Self::parse_impl(blob).with_context(|| {
            match std::str::from_utf8(blob) {
                Ok(s) => format!("Failed to parse: {s}"),
                Err(_) => format!("Failed to parse {blob:?}"),
            }
        })
    }

    fn parse_impl(blob: &[u8]) -> Result<Self> {
        enum Tok {
            Val(JsonValue),
            StartObject,
            StartArray,
        }

        let mut stack = Vec::new();
        let mut i = 0;
        while i < blob.len() {
            match blob[i] {
                b'n' => {
                    i += 4;
                    stack.push(Tok::Val(JsonValue::Null));
                }
                b't' => {
                    i += 4;
                    stack.push(Tok::Val(JsonValue::True));
                }
                b'f' => {
                    i += 5;
                    stack.push(Tok::Val(JsonValue::False));
                }
                b'{' => {
                    i += 1;
                    stack.push(Tok::StartObject);
                }
                b'}' => {
                    i += 1;
                    let mut values = HashMap::new();
                    loop {
                        match stack.pop() {
                            None => bail!("unexpected object terminator"),
                            Some(Tok::StartObject) => break,
                            Some(Tok::StartArray) => {
                                bail!("unterminated array")
                            }
                            Some(Tok::Val(v)) => match stack.pop() {
                                None => bail!(
                                    "unexpected object terminator (mismatch)"
                                ),
                                Some(Tok::StartObject) => {
                                    bail!("mismatch item count")
                                }
                                Some(Tok::StartArray) => {
                                    bail!("unterminated array")
                                }
                                Some(Tok::Val(JsonValue::String(k))) => {
                                    values.insert(k, v);
                                }
                                Some(Tok::Val(_)) => {
                                    bail!("object keys must be strings")
                                }
                            },
                        }
                    }
                    stack.push(Tok::Val(JsonValue::Object(values)));
                }
                b'[' => {
                    i += 1;
                    stack.push(Tok::StartArray);
                }
                b']' => {
                    i += 1;
                    let mut values = Vec::new();
                    loop {
                        match stack.pop() {
                            None => bail!("unexpected array terminator"),
                            Some(Tok::StartObject) => {
                                bail!("unterminated object")
                            }
                            Some(Tok::StartArray) => break,
                            Some(Tok::Val(v)) => values.push(v),
                        }
                    }
                    values.reverse();
                    stack.push(Tok::Val(JsonValue::Array(values)));
                }
                b'"' => {
                    i += 1;
                    let start = i;
                    while i < blob.len() {
                        if blob[i] == b'"' {
                            break;
                        }
                        if blob[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    if i == blob.len() {
                        bail!("Unterminated string at {i}");
                    }
                    assert_eq!(blob[i], b'"');
                    let mut chars =
                        std::str::from_utf8(&blob[start..i])?.chars();
                    i += 1; // Consume the final quote.

                    let mut value = String::new();
                    while let Some(c) = chars.next() {
                        if c == '\\' {
                            match chars.next().expect("mismatched escape") {
                                '"' => value.push('"'),
                                '\\' => value.push('\\'),
                                'b' => value.push('\x08'),
                                'f' => value.push('\x0C'),
                                'n' => value.push('\n'),
                                'r' => value.push('\r'),
                                't' => value.push('\t'),
                                'u' => {
                                    // 4 hex
                                    let mut temp = String::with_capacity(4);
                                    for _ in 0..4 {
                                        let Some(c) = chars.next() else {
                                            bail!("not enough chars in unicode escape")
                                        };
                                        temp.push(c);
                                    }
                                    let code = u32::from_str_radix(&temp, 16)?;
                                    let Some(c) = char::from_u32(code) else {
                                        bail!("invalid escape code {temp}")
                                    };
                                    value.push(c);
                                }
                                _ => bail!("Invalid json escape"),
                            }
                        } else {
                            value.push(c);
                        }
                    }

                    stack.push(Tok::Val(JsonValue::String(value)));
                }
                b',' => i += 1, // Value separator in object or array
                b':' => i += 1, // Key/Value separator
                x if x.is_ascii_whitespace() => i += 1,
                x if x == b'-' || x.is_ascii_digit() => {
                    let start = i;
                    while i < blob.len() {
                        match blob[i] {
                            b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}'
                            | b'[' | b']' | b',' | b':' => {
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                    let number: f64 =
                        std::str::from_utf8(&blob[start..i])?.parse()?;
                    stack.push(Tok::Val(JsonValue::Number(number)));
                }

                x => bail!("Invalid json value start byte {x}"),
            }
        }

        match stack.pop().expect("underflow somehow") {
            Tok::Val(v) => Ok(v),
            Tok::StartObject => bail!("unterminated object"),
            Tok::StartArray => bail!("unterminated array"),
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&HashMap<String, JsonValue>> {
        match self {
            JsonValue::Object(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            JsonValue::String(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            JsonValue::Number(f) => Some(*f),
            _ => None,
        }
    }
}

pub async fn get_entries() -> Result<HashMap<u16, PortDesc>> {
    let mut h: HashMap<u16, PortDesc> = HashMap::new();

    let response = list_containers().await?;
    let response = JsonValue::parse(&response)?;
    let Some(containers) = response.as_array() else {
        bail!("Expected an array of containers")
    };
    for container in containers {
        let Some(container) = container.as_object() else {
            bail!("Expected containers to be objects");
        };

        let name = container
            .get("Names")
            .and_then(|n| n.as_array())
            .and_then(|n| n.first())
            .and_then(|n| n.as_string())
            .unwrap_or("<unknown docker>");

        for port in container
            .get("Ports")
            .and_then(|n| n.as_array())
            .unwrap_or(&[])
        {
            let Some(port) = port.as_object() else {
                bail!("port records must be objects")
            };
            if let Some(public_port) =
                port.get("PublicPort").and_then(|pp| pp.as_number())
            {
                // NOTE: If these are really ports then `as u16` will be
                //       right, otherwise what are we even doing here?
                let public_port = public_port.trunc() as u16;
                let private_port = port
                    .get("PrivatePort")
                    .and_then(|pp| pp.as_number())
                    .unwrap_or(0.0)
                    .trunc() as u16;

                h.insert(
                    public_port,
                    PortDesc {
                        port: public_port,
                        desc: format!("{name} (docker->{private_port})"),
                    },
                );
            }
        }
    }

    Ok(h)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn test_json_decode_basic() {
        let cases: Vec<(&str, JsonValue)> = vec![
            ("12", JsonValue::Number(12.0)),
            ("12.7", JsonValue::Number(12.7)),
            ("-12", JsonValue::Number(-12.0)),
            ("true", JsonValue::True),
            ("false", JsonValue::False),
            ("null", JsonValue::Null),
            ("\"abcd\"", JsonValue::String("abcd".to_string())),
            (
                "\" a \\\" \\r b \\n c \\f d \\b e \\t f \\\\ \"",
                JsonValue::String(
                    " a \" \r b \n c \x0C d \x08 e \t f \\ ".to_string(),
                ),
            ),
        ];
        for (blob, expected) in cases {
            assert_eq!(JsonValue::parse(blob.as_bytes()).unwrap(), expected);
        }
    }

    #[test]
    pub fn test_json_decode_array() {
        let result = JsonValue::parse(b"[1, true, \"foo\", null]").unwrap();
        let JsonValue::Array(result) = result else {
            panic!("Expected an array");
        };
        assert_eq!(result.len(), 4);
        assert_eq!(result[0], JsonValue::Number(1.0));
        assert_eq!(result[1], JsonValue::True);
        assert_eq!(result[2], JsonValue::String("foo".to_owned()));
        assert_eq!(result[3], JsonValue::Null);
    }

    #[test]
    pub fn test_json_decode_array_empty() {
        let result = JsonValue::parse(b"[]").unwrap();
        assert_eq!(result, JsonValue::Array(vec![]));
    }

    #[test]
    pub fn test_json_decode_array_nested() {
        let result = JsonValue::parse(b"[1, [2, 3], 4]").unwrap();
        assert_eq!(
            result,
            JsonValue::Array(vec![
                JsonValue::Number(1.0),
                JsonValue::Array(vec![
                    JsonValue::Number(2.0),
                    JsonValue::Number(3.0),
                ]),
                JsonValue::Number(4.0)
            ])
        );
    }

    #[test]
    pub fn test_json_decode_object() {
        let result = JsonValue::parse(
            b"{\"a\": 1.0, \"b\": [2.0, 3.0], \"c\": {\"d\": 4.0}}",
        )
        .unwrap();
        assert_eq!(
            result,
            JsonValue::Object(HashMap::from([
                ("a".to_owned(), JsonValue::Number(1.0)),
                (
                    "b".to_owned(),
                    JsonValue::Array(vec![
                        JsonValue::Number(2.0),
                        JsonValue::Number(3.0),
                    ])
                ),
                (
                    "c".to_owned(),
                    JsonValue::Object(HashMap::from([(
                        "d".to_owned(),
                        JsonValue::Number(4.0)
                    )]))
                )
            ]))
        )
    }

    #[test]
    pub fn test_json_decode_docker() {
        use pretty_assertions::assert_eq;

        // This is the example container response from docker
        let result = JsonValue::parse(b"
[
  {
    \"Id\": \"8dfafdbc3a40\",
    \"Names\": [
      \"/boring_feynman\"
    ],
    \"Image\": \"ubuntu:latest\",
    \"ImageID\": \"d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82\",
    \"Command\": \"echo 1\",
    \"Created\": 1367854155,
    \"State\": \"Exited\",
    \"Status\": \"Exit 0\",
    \"Ports\": [
      {
        \"PrivatePort\": 2222,
        \"PublicPort\": 3333,
        \"Type\": \"tcp\"
      }
    ],
    \"Labels\": {
      \"com.example.vendor\": \"Acme\",
      \"com.example.license\": \"GPL\",
      \"com.example.version\": \"1.0\"
    },
    \"SizeRw\": 12288,
    \"SizeRootFs\": 0,
    \"HostConfig\": {
      \"NetworkMode\": \"default\",
      \"Annotations\": {
        \"io.kubernetes.docker.type\": \"container\"
      }
    },
    \"NetworkSettings\": {
      \"Networks\": {
        \"bridge\": {
          \"NetworkID\": \"7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812\",
          \"EndpointID\": \"2cdc4edb1ded3631c81f57966563e5c8525b81121bb3706a9a9a3ae102711f3f\",
          \"Gateway\": \"172.17.0.1\",
          \"IPAddress\": \"172.17.0.2\",
          \"IPPrefixLen\": 16,
          \"IPv6Gateway\": \"\",
          \"GlobalIPv6Address\": \"\",
          \"GlobalIPv6PrefixLen\": 0,
          \"MacAddress\": \"02:42:ac:11:00:02\"
        }
      }
    },
    \"Mounts\": [
      {
        \"Name\": \"fac362...80535\",
        \"Source\": \"/data\",
        \"Destination\": \"/data\",
        \"Driver\": \"local\",
        \"Mode\": \"ro,Z\",
        \"RW\": false,
        \"Propagation\": \"\"
      }
    ]
  },
  {
    \"Id\": \"9cd87474be90\",
    \"Names\": [
      \"/coolName\"
    ],
    \"Image\": \"ubuntu:latest\",
    \"ImageID\": \"d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82\",
    \"Command\": \"echo 222222\",
    \"Created\": 1367854155,
    \"State\": \"Exited\",
    \"Status\": \"Exit 0\",
    \"Ports\": [],
    \"Labels\": {},
    \"SizeRw\": 12288,
    \"SizeRootFs\": 0,
    \"HostConfig\": {
      \"NetworkMode\": \"default\",
      \"Annotations\": {
        \"io.kubernetes.docker.type\": \"container\",
        \"io.kubernetes.sandbox.id\": \"3befe639bed0fd6afdd65fd1fa84506756f59360ec4adc270b0fdac9be22b4d3\"
      }
    },
    \"NetworkSettings\": {
      \"Networks\": {
        \"bridge\": {
          \"NetworkID\": \"7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812\",
          \"EndpointID\": \"88eaed7b37b38c2a3f0c4bc796494fdf51b270c2d22656412a2ca5d559a64d7a\",
          \"Gateway\": \"172.17.0.1\",
          \"IPAddress\": \"172.17.0.8\",
          \"IPPrefixLen\": 16,
          \"IPv6Gateway\": \"\",
          \"GlobalIPv6Address\": \"\",
          \"GlobalIPv6PrefixLen\": 0,
          \"MacAddress\": \"02:42:ac:11:00:08\"
        }
      }
    },
    \"Mounts\": []
  },
  {
    \"Id\": \"3176a2479c92\",
    \"Names\": [
      \"/sleepy_dog\"
    ],
    \"Image\": \"ubuntu:latest\",
    \"ImageID\": \"d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82\",
    \"Command\": \"echo 3333333333333333\",
    \"Created\": 1367854154,
    \"State\": \"Exited\",
    \"Status\": \"Exit 0\",
    \"Ports\": [],
    \"Labels\": {},
    \"SizeRw\": 12288,
    \"SizeRootFs\": 0,
    \"HostConfig\": {
      \"NetworkMode\": \"default\",
      \"Annotations\": {
        \"io.kubernetes.image.id\": \"d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82\",
        \"io.kubernetes.image.name\": \"ubuntu:latest\"
      }
    },
    \"NetworkSettings\": {
      \"Networks\": {
        \"bridge\": {
          \"NetworkID\": \"7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812\",
          \"EndpointID\": \"8b27c041c30326d59cd6e6f510d4f8d1d570a228466f956edf7815508f78e30d\",
          \"Gateway\": \"172.17.0.1\",
          \"IPAddress\": \"172.17.0.6\",
          \"IPPrefixLen\": 16,
          \"IPv6Gateway\": \"\",
          \"GlobalIPv6Address\": \"\",
          \"GlobalIPv6PrefixLen\": 0,
          \"MacAddress\": \"02:42:ac:11:00:06\"
        }
      }
    },
    \"Mounts\": []
  },
  {
    \"Id\": \"4cb07b47f9fb\",
    \"Names\": [
      \"/running_cat\"
    ],
    \"Image\": \"ubuntu:latest\",
    \"ImageID\": \"d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82\",
    \"Command\": \"echo 444444444444444444444444444444444\",
    \"Created\": 1367854152,
    \"State\": \"Exited\",
    \"Status\": \"Exit 0\",
    \"Ports\": [],
    \"Labels\": {},
    \"SizeRw\": 12288,
    \"SizeRootFs\": 0,
    \"HostConfig\": {
      \"NetworkMode\": \"default\",
      \"Annotations\": {
        \"io.kubernetes.config.source\": \"api\"
      }
    },
    \"NetworkSettings\": {
      \"Networks\": {
        \"bridge\": {
          \"NetworkID\": \"7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812\",
          \"EndpointID\": \"d91c7b2f0644403d7ef3095985ea0e2370325cd2332ff3a3225c4247328e66e9\",
          \"Gateway\": \"172.17.0.1\",
          \"IPAddress\": \"172.17.0.5\",
          \"IPPrefixLen\": 16,
          \"IPv6Gateway\": \"\",
          \"GlobalIPv6Address\": \"\",
          \"GlobalIPv6PrefixLen\": 0,
          \"MacAddress\": \"02:42:ac:11:00:05\"
        }
      }
    },
    \"Mounts\": []
  }
]
").unwrap();
        let expected = JsonValue::Array(vec![
            JsonValue::Object(HashMap::from([
                ("Id".to_owned(), JsonValue::String("8dfafdbc3a40".to_owned())),
                ("Names".to_owned(), JsonValue::Array(vec![
                    JsonValue::String("/boring_feynman".to_owned())
                ])),
                ("Image".to_owned(), JsonValue::String("ubuntu:latest".to_owned())),
                ("ImageID".to_owned(), JsonValue::String("d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82".to_owned())),
                ("Command".to_owned(), JsonValue::String("echo 1".to_owned())),
                ("Created".to_owned(), JsonValue::Number(1367854155_f64)),
                ("State".to_owned(), JsonValue::String("Exited".to_owned())),
                ("Status".to_owned(), JsonValue::String("Exit 0".to_owned())),
                ("Ports".to_owned(), JsonValue::Array(vec![
                    JsonValue::Object(HashMap::from([
                        ("PrivatePort".to_owned(), JsonValue::Number(2222_f64)),
                        ("PublicPort".to_owned(), JsonValue::Number(3333_f64)),
                        ("Type".to_owned(), JsonValue::String("tcp".to_owned()))
                    ]))
                ])),
                ("Labels".to_owned(), JsonValue::Object(HashMap::from([
                    ("com.example.vendor".to_owned(), JsonValue::String("Acme".to_owned())),
                    ("com.example.license".to_owned(), JsonValue::String("GPL".to_owned())),
                    ("com.example.version".to_owned(), JsonValue::String("1.0".to_owned()))
                ]))),
                ("SizeRw".to_owned(), JsonValue::Number(12288_f64)),
                ("SizeRootFs".to_owned(), JsonValue::Number(0_f64)),
                ("HostConfig".to_owned(), JsonValue::Object(HashMap::from([
                    ("NetworkMode".to_owned(), JsonValue::String("default".to_owned())),
                    ("Annotations".to_owned(), JsonValue::Object(HashMap::from([
                        ("io.kubernetes.docker.type".to_owned(), JsonValue::String("container".to_owned()))
                    ])))
                ]))),
                ("NetworkSettings".to_owned(), JsonValue::Object(HashMap::from([
                    ("Networks".to_owned(), JsonValue::Object(HashMap::from([
                        ("bridge".to_owned(), JsonValue::Object(HashMap::from([
                            ("NetworkID".to_owned(), JsonValue::String("7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812".to_owned())),
                            ("EndpointID".to_owned(), JsonValue::String("2cdc4edb1ded3631c81f57966563e5c8525b81121bb3706a9a9a3ae102711f3f".to_owned())),
                            ("Gateway".to_owned(), JsonValue::String("172.17.0.1".to_owned())),
                            ("IPAddress".to_owned(), JsonValue::String("172.17.0.2".to_owned())),
                            ("IPPrefixLen".to_owned(), JsonValue::Number(16_f64)),
                            ("IPv6Gateway".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6Address".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6PrefixLen".to_owned(), JsonValue::Number(0_f64)),
                            ("MacAddress".to_owned(), JsonValue::String("02:42:ac:11:00:02".to_owned()))
                        ])))
                    ])))
                ]))),
                ("Mounts".to_owned(), JsonValue::Array(vec![
                    JsonValue::Object(HashMap::from([
                        ("Name".to_owned(), JsonValue::String("fac362...80535".to_owned())),
                        ("Source".to_owned(), JsonValue::String("/data".to_owned())),
                        ("Destination".to_owned(), JsonValue::String("/data".to_owned())),
                        ("Driver".to_owned(), JsonValue::String("local".to_owned())),
                        ("Mode".to_owned(), JsonValue::String("ro,Z".to_owned())),
                        ("RW".to_owned(), JsonValue::False),
                        ("Propagation".to_owned(), JsonValue::String("".to_owned()))
                    ]))
                ]))
            ])),
            JsonValue::Object(HashMap::from([
                ("Id".to_owned(), JsonValue::String("9cd87474be90".to_owned())),
                ("Names".to_owned(), JsonValue::Array(vec![
                    JsonValue::String("/coolName".to_owned())
                ])),
                ("Image".to_owned(), JsonValue::String("ubuntu:latest".to_owned())),
                ("ImageID".to_owned(), JsonValue::String("d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82".to_owned())),
                ("Command".to_owned(), JsonValue::String("echo 222222".to_owned())),
                ("Created".to_owned(), JsonValue::Number(1367854155_f64)),
                ("State".to_owned(), JsonValue::String("Exited".to_owned())),
                ("Status".to_owned(), JsonValue::String("Exit 0".to_owned())),
                ("Ports".to_owned(), JsonValue::Array(vec![])),
                ("Labels".to_owned(), JsonValue::Object(HashMap::from([]))),
                ("SizeRw".to_owned(), JsonValue::Number(12288_f64)),
                ("SizeRootFs".to_owned(), JsonValue::Number(0_f64)),
                ("HostConfig".to_owned(), JsonValue::Object(HashMap::from([
                    ("NetworkMode".to_owned(), JsonValue::String("default".to_owned())),
                    ("Annotations".to_owned(), JsonValue::Object(HashMap::from([
                        ("io.kubernetes.docker.type".to_owned(), JsonValue::String("container".to_owned())),
                        ("io.kubernetes.sandbox.id".to_owned(), JsonValue::String("3befe639bed0fd6afdd65fd1fa84506756f59360ec4adc270b0fdac9be22b4d3".to_owned()))
                    ])))
                ]))),
                ("NetworkSettings".to_owned(), JsonValue::Object(HashMap::from([
                    ("Networks".to_owned(), JsonValue::Object(HashMap::from([
                        ("bridge".to_owned(), JsonValue::Object(HashMap::from([
                            ("NetworkID".to_owned(), JsonValue::String("7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812".to_owned())),
                            ("EndpointID".to_owned(), JsonValue::String("88eaed7b37b38c2a3f0c4bc796494fdf51b270c2d22656412a2ca5d559a64d7a".to_owned())),
                            ("Gateway".to_owned(), JsonValue::String("172.17.0.1".to_owned())),
                            ("IPAddress".to_owned(), JsonValue::String("172.17.0.8".to_owned())),
                            ("IPPrefixLen".to_owned(), JsonValue::Number(16_f64)),
                            ("IPv6Gateway".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6Address".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6PrefixLen".to_owned(), JsonValue::Number(0_f64)),
                            ("MacAddress".to_owned(), JsonValue::String("02:42:ac:11:00:08".to_owned()))
                        ])))
                    ])))
                ]))),
                ("Mounts".to_owned(), JsonValue::Array(vec![]))
            ])),
            JsonValue::Object(HashMap::from([
                ("Id".to_owned(), JsonValue::String("3176a2479c92".to_owned())),
                ("Names".to_owned(), JsonValue::Array(vec![
                    JsonValue::String("/sleepy_dog".to_owned())
                ])),
                ("Image".to_owned(), JsonValue::String("ubuntu:latest".to_owned())),
                ("ImageID".to_owned(), JsonValue::String("d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82".to_owned())),
                ("Command".to_owned(), JsonValue::String("echo 3333333333333333".to_owned())),
                ("Created".to_owned(), JsonValue::Number(1367854154_f64)),
                ("State".to_owned(), JsonValue::String("Exited".to_owned())),
                ("Status".to_owned(), JsonValue::String("Exit 0".to_owned())),
                ("Ports".to_owned(), JsonValue::Array(vec![])),
                ("Labels".to_owned(), JsonValue::Object(HashMap::from([]))),
                ("SizeRw".to_owned(), JsonValue::Number(12288_f64)),
                ("SizeRootFs".to_owned(), JsonValue::Number(0_f64)),
                ("HostConfig".to_owned(), JsonValue::Object(HashMap::from([
                    ("NetworkMode".to_owned(), JsonValue::String("default".to_owned())),
                    ("Annotations".to_owned(), JsonValue::Object(HashMap::from([
                        ("io.kubernetes.image.id".to_owned(), JsonValue::String("d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82".to_owned())),
                        ("io.kubernetes.image.name".to_owned(), JsonValue::String("ubuntu:latest".to_owned()))
                    ])))
                ]))),
                ("NetworkSettings".to_owned(), JsonValue::Object(HashMap::from([
                    ("Networks".to_owned(), JsonValue::Object(HashMap::from([
                        ("bridge".to_owned(), JsonValue::Object(HashMap::from([
                            ("NetworkID".to_owned(), JsonValue::String("7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812".to_owned())),
                            ("EndpointID".to_owned(), JsonValue::String("8b27c041c30326d59cd6e6f510d4f8d1d570a228466f956edf7815508f78e30d".to_owned())),
                            ("Gateway".to_owned(), JsonValue::String("172.17.0.1".to_owned())),
                            ("IPAddress".to_owned(), JsonValue::String("172.17.0.6".to_owned())),
                            ("IPPrefixLen".to_owned(), JsonValue::Number(16_f64)),
                            ("IPv6Gateway".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6Address".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6PrefixLen".to_owned(), JsonValue::Number(0_f64)),
                            ("MacAddress".to_owned(), JsonValue::String("02:42:ac:11:00:06".to_owned()))
                        ])))
                    ])))
                ]))),
                ("Mounts".to_owned(), JsonValue::Array(vec![]))
            ])),
            JsonValue::Object(HashMap::from([
                ("Id".to_owned(), JsonValue::String("4cb07b47f9fb".to_owned())),
                ("Names".to_owned(), JsonValue::Array(vec![
                    JsonValue::String("/running_cat".to_owned())
                ])),
                ("Image".to_owned(), JsonValue::String("ubuntu:latest".to_owned())),
                ("ImageID".to_owned(), JsonValue::String("d74508fb6632491cea586a1fd7d748dfc5274cd6fdfedee309ecdcbc2bf5cb82".to_owned())),
                ("Command".to_owned(), JsonValue::String("echo 444444444444444444444444444444444".to_owned())),
                ("Created".to_owned(), JsonValue::Number(1367854152_f64)),
                ("State".to_owned(), JsonValue::String("Exited".to_owned())),
                ("Status".to_owned(), JsonValue::String("Exit 0".to_owned())),
                ("Ports".to_owned(), JsonValue::Array(vec![])),
                ("Labels".to_owned(), JsonValue::Object(HashMap::from([]))),
                ("SizeRw".to_owned(), JsonValue::Number(12288_f64)),
                ("SizeRootFs".to_owned(), JsonValue::Number(0_f64)),
                ("HostConfig".to_owned(), JsonValue::Object(HashMap::from([
                    ("NetworkMode".to_owned(), JsonValue::String("default".to_owned())),
                    ("Annotations".to_owned(), JsonValue::Object(HashMap::from([
                        ("io.kubernetes.config.source".to_owned(), JsonValue::String("api".to_owned()))
                    ])))
                ]))),
                ("NetworkSettings".to_owned(), JsonValue::Object(HashMap::from([
                    ("Networks".to_owned(), JsonValue::Object(HashMap::from([
                        ("bridge".to_owned(), JsonValue::Object(HashMap::from([
                            ("NetworkID".to_owned(), JsonValue::String("7ea29fc1412292a2d7bba362f9253545fecdfa8ce9a6e37dd10ba8bee7129812".to_owned())),
                            ("EndpointID".to_owned(), JsonValue::String("d91c7b2f0644403d7ef3095985ea0e2370325cd2332ff3a3225c4247328e66e9".to_owned())),
                            ("Gateway".to_owned(), JsonValue::String("172.17.0.1".to_owned())),
                            ("IPAddress".to_owned(), JsonValue::String("172.17.0.5".to_owned())),
                            ("IPPrefixLen".to_owned(), JsonValue::Number(16_f64)),
                            ("IPv6Gateway".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6Address".to_owned(), JsonValue::String("".to_owned())),
                            ("GlobalIPv6PrefixLen".to_owned(), JsonValue::Number(0_f64)),
                            ("MacAddress".to_owned(), JsonValue::String("02:42:ac:11:00:05".to_owned()))
                        ])))
                    ])))
                ]))),
                ("Mounts".to_owned(), JsonValue::Array(vec![]))
            ]))
        ]);
        assert_eq!(result, expected);
    }
}
