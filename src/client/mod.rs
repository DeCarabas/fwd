use crate::message::{Message, MessageReader, MessageWriter};
use anyhow::{bail, Result};
use bytes::BytesMut;
use log::LevelFilter;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite,
    AsyncWriteExt, BufReader, BufWriter,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::sync::mpsc;

mod config;
mod ui;

// 256MB clipboard. Operating systems do not generally have a maximum size
// but I need to buffer it all in memory in order to use the `copypasta`
// crate and so I want to set a limit somewhere and this seems.... rational.
// You should use another transfer mechanism other than this if you need more
// than 256 MB of data. (Obviously future me will come along and shake his
// head at the limitations of my foresight, but oh well.)
const MAX_CLIPBOARD_SIZE: usize = 256 * 1024 * 1024;

/// Wait for the server to be ready; we know the server is there and
/// listening when we see the special sync marker, which is 8 NUL bytes in a
/// row.
async fn client_sync<S: AsyncRead + Unpin, T: AsyncRead + Unpin>(
    reader: &mut S,
    client_stderr: &mut T,
) -> Result<(), tokio::io::Error> {
    info!("Waiting for synchronization marker...");

    let mut stderr = tokio::io::stderr();
    let mut stdout = tokio::io::stdout();
    let mut buf = BytesMut::with_capacity(1024);

    let mut seen = 0;
    let result = tokio::select! {
        result = async {
            loop {
                buf.clear();
                client_stderr.read_buf(&mut buf).await?;
                stderr.write_all(&buf[..]).await?;
            }
        } => result,
        result = async {
            while seen < 8 {
                let byte = reader.read_u8().await?;
                if byte == 0 {
                    seen += 1;
                } else {
                    stdout.write_u8(byte).await?;
                    seen = 0;
                }
            }

            Ok::<_, tokio::io::Error>(())
        } => result,
    };

    if result.is_err() {
        // Something went wrong, let's just make sure we flush the client's
        // stderr before we return.
        _ = stderr.write_all(&buf[..]).await;
        _ = tokio::io::copy(client_stderr, &mut stderr).await;
        _ = stderr.flush().await;
    }

    result
}

/// Handle an incoming client connection, by forwarding it to the SOCKS5
/// server at the specified port. This is the core of the entire thing.
///
/// This contains a very simplified implementation of a SOCKS5 connector,
/// enough to work with the SSH I have. I would have liked it to be SOCKS4,
/// which is a much simpler protocol, but somehow it didn't work.
async fn client_handle_connection(
    socks_port: u16,
    port: u16,
    socket: TcpStream,
) -> Result<()> {
    debug!("Handling connection!");

    let dest_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, socks_port);
    let mut dest_socket = TcpStream::connect(dest_addr).await?;

    debug!("Connected, sending handshake request");
    let packet: [u8; 3] = [
        0x05, // v5
        0x01, // 1 auth method
        0x00, // my one auth method is no auth
    ];
    dest_socket.write_all(&packet[..]).await?;
    debug!("Initial handshake sent. Awaiting handshake response");

    let mut response: [u8; 2] = [0; 2];
    dest_socket.read_exact(&mut response).await?;
    if response[0] != 0x05 {
        bail!("SOCKS incorrect response version {}", response[0]);
    }
    if response[1] == 0xFF {
        bail!("SOCKS server says no acceptable auth");
    }
    if response[1] != 0x00 {
        bail!("SOCKS server chose something wild? {}", response[1]);
    }

    debug!("Handshake response received, sending connect request");
    let packet: [u8; 10] = [
        0x05,                                       // version again :P
        0x01,                                       // connect
        0x00,                                       // reserved!
        0x01,                                       // ipv4
        127,                                        // lo..
        0,                                          // ..cal..
        0,                                          // ..ho..
        1,                                          // ..st
        ((port & 0xFF00) >> 8).try_into().unwrap(), // port (high)
        (port & 0x00FF).try_into().unwrap(),        // port (low)
    ];
    dest_socket.write_all(&packet[..]).await?;

    debug!("Connect request sent, awaiting response");
    let mut response: [u8; 4] = [0; 4];
    dest_socket.read_exact(&mut response).await?;
    if response[0] != 0x05 {
        bail!("SOCKS5 incorrect response version again? {}", response[0]);
    }
    if response[1] != 0x00 {
        bail!("SOCKS5 reports a connect error {}", response[1]);
    }
    // Now we 100% do not care about the following information but we must
    // discard it so we can get to the good stuff. response[3] is the type of
    // address...
    if response[3] == 0x01 {
        // IPv4 - 4 bytes.
        let mut response: [u8; 4] = [0; 4];
        dest_socket.read_exact(&mut response).await?;
    } else if response[3] == 0x03 {
        // Domain Name
        let len = dest_socket.read_u8().await?;
        for _ in 0..len {
            dest_socket.read_u8().await?; // So slow!
        }
    } else if response[3] == 0x04 {
        // IPv6 - 8 bytes
        let mut response: [u8; 8] = [0; 8];
        dest_socket.read_exact(&mut response).await?;
    } else {
        bail!(
            "SOCKS5 sent me an address I don't understand {}",
            response[3]
        );
    }
    // Finally the port number. Again, garbage, but it's in the packet we
    // need to skip.
    let mut response: [u8; 2] = [0; 2];
    dest_socket.read_exact(&mut response).await?;

    info!("Connection established on port {}", port);

    let mut socket = socket;
    tokio::io::copy_bidirectional(&mut socket, &mut dest_socket).await?;
    Ok(())
}

/// Listen on a port that we are currently forwarding, and use the SOCKS5
/// proxy on the specified port to handle the connections.
async fn client_listen(port: u16, socks_port: u16) -> Result<()> {
    let listener =
        TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
    loop {
        // The second item contains the IP and port of the new
        // connection, but we don't care.
        let (socket, _) = listener.accept().await?;

        tokio::spawn(async move {
            if let Err(e) =
                client_handle_connection(socks_port, port, socket).await
            {
                error!("Error handling connection: {:?}", e);
            } else {
                debug!("Done???");
            }
        });
    }
}

async fn client_handle_messages<T: AsyncRead + Unpin>(
    mut reader: MessageReader<T>,
    events: mpsc::Sender<ui::UIEvent>,
) -> Result<()> {
    let mut clipboard_messages = HashMap::new();
    loop {
        use Message::*;
        match reader.read().await? {
            Ping => (),

            Ports(ports) => {
                if let Err(e) = events.send(ui::UIEvent::Ports(ports)).await {
                    error!("Error sending ports request: {:?}", e);
                }
            }

            Browse(url) => {
                info!("Browsing to {url}...");
                _ = open::that(url);
            }

            ClipStart(id) => {
                clipboard_messages.insert(id, Vec::new());
            }

            ClipData(id, mut data) => match clipboard_messages.get_mut(&id) {
                Some(bytes) => {
                    if bytes.len() < MAX_CLIPBOARD_SIZE {
                        bytes.append(&mut data);
                    }
                }
                None => {
                    warn!("Received data for unknown clip op {id}");
                }
            },

            ClipEnd(id) => {
                let Some(data) = clipboard_messages.remove(&id) else {
                    warn!("Received end for unknown clip op {id}");
                    continue;
                };

                let Ok(data) = String::from_utf8(data) else {
                    warn!("Received invalid utf8 for clipboard on op {id}");
                    continue;
                };

                if let Err(e) =
                    events.send(ui::UIEvent::SetClipboard(data)).await
                {
                    error!("Error sending clipboard request: {:?}", e);
                }
            }

            message => error!("Unsupported: {:?}", message),
        };
    }
}

async fn client_pipe_stderr<Debug: AsyncBufRead + Unpin>(
    debug: &mut Debug,
    events: mpsc::Sender<ui::UIEvent>,
) {
    loop {
        let mut line = String::new();
        match debug.read_line(&mut line).await {
            Err(e) => {
                error!("Error reading stderr from server: {:?}", e);
                break;
            }
            Ok(0) => {
                warn!("stderr stream closed");
                break;
            }
            _ => {
                _ = events.send(ui::UIEvent::ServerLine(line)).await;
            }
        }
    }
}

async fn client_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    socks_port: u16,
    mut reader: MessageReader<Reader>,
    mut writer: MessageWriter<Writer>,
    events: mpsc::Sender<ui::UIEvent>,
) -> Result<()> {
    // Wait for the server's announcement.
    if let Message::Hello(major, minor, _) = reader.read().await? {
        info!("Server Version: {major} {minor}");
        if major != 0 || minor > 2 {
            bail!("Unsupported remote protocol version {}.{}", major, minor);
        }
    } else {
        bail!("Expected a hello message from the remote server");
    }

    // And now really get into it...
    _ = events.send(ui::UIEvent::Connected(socks_port)).await;

    tokio::select! {
        result = async {
            loop {
                use tokio::time::{sleep, Duration};
                if let Err(e) = writer.write(Message::Refresh).await {
                    break Err::<(), _>(e);
                }
                sleep(Duration::from_millis(500)).await;
            }
        } => {
            if let Err(e) = result {
                println!("Error sending refreshes");
                return Err(e.into());
            }
        },
        result = client_handle_messages(reader, events) => {
            if let Err(e) = result {
                println!("Error handling messages");
                return Err(e);
            }
        },
    }
    Ok(())
}

async fn spawn_ssh(
    server: &str,
    sudo: bool,
    log_filter: &str,
) -> Result<(tokio::process::Child, u16), std::io::Error> {
    let socks_port = {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        listener.local_addr()?.port()
    };

    let mut cmd = process::Command::new("ssh");
    cmd.arg("-T")
        .arg("-D")
        .arg(socks_port.to_string())
        .arg(server);
    if sudo {
        cmd.arg("sudo");
    }
    cmd.arg(format!("FWD_LOG={log_filter}"))
        .arg("FWD_SEND_ANONYMOUS=1")
        .arg("fwd")
        .arg("--server");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = cmd.spawn()?;
    Ok((child, socks_port))
}

#[cfg(target_family = "windows")]
fn is_sigint(status: std::process::ExitStatus) -> bool {
    match status.code() {
        Some(255) => true,
        _ => false,
    }
}

#[cfg(target_family = "unix")]
fn is_sigint(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(2) => true,
        Some(_) => false,
        None => false,
    }
}

async fn client_connect_loop(
    remote: &str,
    sudo: bool,
    log_filter: &str,
    events: mpsc::Sender<ui::UIEvent>,
) {
    loop {
        _ = events.send(ui::UIEvent::Disconnected).await;

        let (mut child, socks_port) = spawn_ssh(remote, sudo, log_filter)
            .await
            .expect("failed to spawn");

        let mut stderr = child
            .stderr
            .take()
            .expect("child did not have a handle to stderr");

        let writer = child
            .stdin
            .take()
            .expect("child did not have a handle to stdin");

        let mut reader = BufReader::new(
            child
                .stdout
                .take()
                .expect("child did not have a handle to stdout"),
        );

        if let Err(e) = client_sync(&mut reader, &mut stderr).await {
            error!("Error synchronizing: {:?}", e);
            if let Ok(status) = child.wait().await {
                if is_sigint(status) {
                    return;
                } else if let Some(127) = status.code() {
                    eprintln!(
                        "Cannot find `fwd` remotely, make sure it is installed"
                    );
                }
            };

            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            continue;
        }

        let mut stderr = BufReader::new(stderr);
        let writer = MessageWriter::new(BufWriter::new(writer));
        let reader = MessageReader::new(reader);

        let sec = events.clone();
        tokio::spawn(async move {
            client_pipe_stderr(&mut stderr, sec).await;
        });

        if let Err(e) =
            client_main(socks_port, reader, writer, events.clone()).await
        {
            error!("Server disconnected with error: {:?}", e);
        } else {
            warn!("Disconnected from server, reconnecting...");
        }
    }
}

pub async fn run_client(remote: &str, sudo: bool, log_filter: &str) {
    let (event_sender, event_receiver) = mpsc::channel(1024);
    _ = log::set_boxed_logger(ui::Logger::new(event_sender.clone()));
    log::set_max_level(LevelFilter::Info);

    let server = if let Some((_user, server)) = remote.split_once("@") {
        server
    } else {
        remote
    };
    let config = match config::load_config() {
        Ok(config) => config.get(server),
        Err(e) => {
            eprintln!("Error loading configuration: {:?}", e);
            return;
        }
    };

    let mut ui = ui::UI::new(event_receiver, config);

    // Start the reconnect loop.
    tokio::select! {
        _ = ui.run() => (),
        _ = client_connect_loop(remote, sudo, log_filter, event_sender) => ()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use tokio::io::DuplexStream;
    use tokio::sync::mpsc::Receiver;

    struct Fixture {
        _server_read: MessageReader<DuplexStream>,
        server_write: MessageWriter<DuplexStream>,
        _event_receiver: Receiver<ui::UIEvent>,
        client_result: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    }

    impl Fixture {
        pub fn new() -> Self {
            let (server_read, client_write) = tokio::io::duplex(4096);
            let server_read = MessageReader::new(server_read);
            let client_write = MessageWriter::new(client_write);

            let (client_read, server_write) = tokio::io::duplex(4096);
            let client_read = MessageReader::new(client_read);
            let server_write = MessageWriter::new(server_write);

            let (event_sender, event_receiver) = mpsc::channel(1024);

            let client_result = tokio::spawn(async move {
                client_main(0, client_read, client_write, event_sender).await
            });

            Fixture {
                _server_read: server_read,
                server_write,
                _event_receiver: event_receiver,
                client_result: Some(client_result),
            }
        }

        pub async fn shutdown(mut self) -> anyhow::Result<()> {
            let result = self.client_result.take();
            drop(self); // Side effect: close all streams.
            result.unwrap().await.expect("Unexpected join error")
        }
    }

    #[tokio::test]
    async fn basic_hello_sync() {
        let mut t = Fixture::new();

        t.server_write
            .write(Message::Hello(0, 2, vec![]))
            .await
            .expect("Error sending hello");
    }

    #[tokio::test]
    async fn basic_hello_high_minor() {
        let mut t = Fixture::new();

        t.server_write
            .write(Message::Hello(0, 99, vec![]))
            .await
            .expect("Error sending hello");

        assert_matches!(t.shutdown().await, Err(_));
    }

    #[tokio::test]
    async fn basic_hello_wrong_major() {
        let mut t = Fixture::new();

        t.server_write
            .write(Message::Hello(99, 0, vec![]))
            .await
            .expect("Error sending hello");

        assert_matches!(t.shutdown().await, Err(_));
    }
}
