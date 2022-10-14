use anyhow::{bail, Result};
use connection::ConnectionTable;
use log::LevelFilter;
use log::{error, info, warn};
use message::{Message, MessageReader, MessageWriter};
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    BufWriter,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

mod connection;
mod message;
mod refresh;
mod ui;

// ----------------------------------------------------------------------------
// Write Management

/// Gathers writes from an mpsc queue and writes them to the specified
/// writer.
///
/// This is kind of an odd function. It raises a lot of questions.
///
/// *Why can't this just be a wrapper function on top of MessageWriter that
/// everybody calls?* Well, we could do that, but we also need to synchronize
/// writes to the underlying stream.
///
/// *Why not use an async mutex?* Because this function has a nice side
/// benefit: if it ever quits, we're *either* doing an orderly shutdown
/// (because the last write end of this channel closed) *or* the remote
/// connection has closed. [client_main] uses this fact to its advantage to
/// detect when the connection has failed.
///
/// At some point we may even automatically reconnect in response!
///
async fn pump_write<T: AsyncWrite + Unpin>(
    messages: &mut mpsc::Receiver<Message>,
    writer: &mut MessageWriter<T>,
) -> Result<()> {
    while let Some(msg) = messages.recv().await {
        writer.write(msg).await?;
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Server

async fn server_handle_connection(
    channel: u64,
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
) {
    let mut connections = connections;
    if let Ok(mut stream) = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await {
        let (send_data, mut data) = mpsc::channel(32);
        connections.add(channel, send_data);
        if let Ok(_) = writer.send(Message::Connected(channel)).await {
            let mut writer = writer.clone();
            connection::process(channel, &mut stream, &mut data, &mut writer).await;

            info!("< Done server!");
        }
    }
}

async fn server_read<T: AsyncRead + Unpin>(
    reader: &mut MessageReader<T>,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
) -> Result<()> {
    // info!("< Processing packets...");
    loop {
        let message = reader.read().await?;

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
                        Err(_e) => {
                            error!("< Error scanning: {:?}", _e);
                            vec![]
                        }
                    };
                    if let Err(_e) = writer.send(Message::Ports(ports)).await {
                        // Writer has been closed for some reason, we can just quit.... I hope everything is OK?
                        warn!("< Warning: Error sending: {:?}", _e);
                    }
                });
            }
            _ => panic!("Unsupported: {:?}", message),
        };
    }
}

async fn server_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    reader: &mut MessageReader<Reader>,
    writer: &mut MessageWriter<Writer>,
) -> Result<()> {
    let connections = ConnectionTable::new();

    // The first message we send must be an announcement.
    writer.write(Message::Hello(0, 1, vec![])).await?;

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

async fn client_sync<Read: AsyncRead + Unpin>(reader: &mut Read) -> Result<(), tokio::io::Error> {
    info!("Waiting for synchronization marker...");

    // Run these two loops in parallel; the copy of stdin should stop when
    // we've seen the marker from the client. If the pipe closes for whatever
    // reason then obviously we quit.
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = async {
            let mut seen = 0;
            while seen < 8 {
                let byte = reader.read_u8().await?;
                if byte == 0 {
                    seen += 1;
                } else {
                    stdout.write_u8(byte).await?;
                }
            }

            Ok::<_, tokio::io::Error>(())
        } => result,
    }
}

async fn client_handle_connection(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
    socket: &mut TcpStream,
) {
    let mut connections = connections;
    let (send_connected, connected) = oneshot::channel();
    let (send_data, mut data) = mpsc::channel(32);
    let channel = connections.alloc(send_connected, send_data);

    if let Ok(_) = writer.send(Message::Connect(channel, port)).await {
        if let Ok(_) = connected.await {
            let mut writer = writer.clone();
            connection::process(channel, socket, &mut data, &mut writer).await;
        } else {
            error!("Failed to connect to remote");
        }
    }
}

async fn client_listen(
    port: u16,
    writer: mpsc::Sender<Message>,
    connections: ConnectionTable,
) -> Result<()> {
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
    reader: &mut MessageReader<T>,
    connections: ConnectionTable,
    events: mpsc::Sender<ui::UIEvent>,
) -> Result<()> {
    info!("Running");
    loop {
        let message = reader.read().await?;
        // info!("> packet {:?}", message); // TODO: Smaller

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
                if let Err(_) = events.send(ui::UIEvent::Ports(ports)).await {
                    // TODO: Log
                }
            }
            _ => panic!("Unsupported: {:?}", message),
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
    reader: &mut MessageReader<Reader>,
    writer: &mut MessageWriter<Writer>,
    events: mpsc::Sender<ui::UIEvent>,
) -> Result<()> {
    // Wait for the server's announcement.
    if let Message::Hello(major, minor, _) = reader.read().await? {
        if major != 0 || minor > 1 {
            bail!("Unsupported remote protocol version {}.{}", major, minor);
        }
    } else {
        bail!("Expected a hello message from the remote server");
    }

    let connections = ConnectionTable::new();

    // And now really get into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);

    _ = events
        .send(ui::UIEvent::Connected(
            msg_sender.clone(),
            connections.clone(),
        ))
        .await;

    let writing = pump_write(&mut msg_receiver, writer);
    let reading = client_read(reader, connections, events);
    tokio::pin!(reading);
    tokio::pin!(writing);

    let (mut done_writing, mut done_reading) = (false, false);
    while !(done_reading && done_writing) {
        tokio::select! {
            result = async {
                loop {
                    use tokio::time::{sleep, Duration};
                    if let Err(e) = msg_sender.send(Message::Refresh).await {
                        break Err::<(), _>(e);
                    }
                    sleep(Duration::from_millis(500)).await;
                }
            }, if !done_writing => {
                if let Err(e) = result {
                    return Err(e.into());
                }
            },
            result = &mut writing, if !done_writing => {
                done_writing = true;
                if let Err(e) = result {
                    return Err(e);
                }
            },
            result = &mut reading, if !done_reading => {
                done_reading = true;
                if let Err(e) = result {
                    return Err(e);
                }
            },
        }
    }
    Ok(())
}

/////

pub async fn run_server() {
    let reader = BufReader::new(tokio::io::stdin());
    let mut writer = BufWriter::new(tokio::io::stdout());

    // Write the 8-byte synchronization marker.
    // eprintln!("< Writing marker...");
    writer
        .write_u64(0x00_00_00_00_00_00_00_00)
        .await
        .expect("Error writing marker");

    if let Err(_) = writer.flush().await {
        // eprintln!("Error writing sync marker: {:?}", e);
        return;
    }
    // eprintln!("< Done!");

    let mut writer = MessageWriter::new(writer);
    let mut reader = MessageReader::new(reader);
    if let Err(_) = server_main(&mut reader, &mut writer).await {
        // eprintln!("Error: {:?}", e);
    }
}

async fn spawn_ssh(server: &str) -> Result<tokio::process::Child, std::io::Error> {
    let mut cmd = process::Command::new("ssh");
    cmd.arg("-T").arg(server).arg("fwd").arg("--server");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.spawn()
}

async fn client_connect_loop(remote: &str, events: mpsc::Sender<ui::UIEvent>) {
    loop {
        _ = events.send(ui::UIEvent::Disconnected).await;

        let mut child = spawn_ssh(remote).await.expect("failed to spawn");

        let mut stderr = BufReader::new(
            child
                .stderr
                .take()
                .expect("child did not have a handle to stderr"),
        );

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

        if let Err(e) = client_sync(&mut reader).await {
            error!("Error synchronizing: {:?}", e);
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            continue;
        }

        let mut writer = MessageWriter::new(BufWriter::new(writer));
        let mut reader = MessageReader::new(reader);

        let sec = events.clone();
        tokio::spawn(async move {
            client_pipe_stderr(&mut stderr, sec).await;
        });

        if let Err(e) = client_main(&mut reader, &mut writer, events.clone()).await {
            error!("Server disconnected with error: {:?}", e);
        } else {
            warn!("Disconnected from server, reconnecting...");
        }
    }
}

pub async fn run_client(remote: &str) {
    let (event_sender, event_receiver) = mpsc::channel(1024);
    _ = log::set_boxed_logger(ui::Logger::new(event_sender.clone()));
    log::set_max_level(LevelFilter::Info);

    let mut ui = ui::UI::new(event_receiver);

    // Start the reconnect loop.
    tokio::select! {
        _ = ui.run() => (),
        _ = client_connect_loop(remote, event_sender) => ()
    }
}
