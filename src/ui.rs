use crate::client_listen;
use crate::{
    message::{Message, PortDesc},
    ConnectionTable,
};
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{Event, EventStream, KeyCode, KeyEvent},
    execute, queue,
    style::{Color, PrintStyledContent, Stylize},
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType, DisableLineWrap, EnableLineWrap,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use log::{error, info, Level, Metadata, Record};
use open;
use std::collections::vec_deque::VecDeque;
use std::collections::HashMap;
use std::io::{stdout, Write};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;

pub enum UIEvent {
    Connected(mpsc::Sender<Message>, ConnectionTable),
    Disconnected,
    ServerLine(String),
    LogLine(log::Level, String),
    Ports(Vec<PortDesc>),
}

#[derive(Debug, Clone)]
pub struct Logger {
    line_sender: mpsc::Sender<UIEvent>,
}

impl Logger {
    pub fn new(line_sender: mpsc::Sender<UIEvent>) -> Box<Logger> {
        Box::new(Logger { line_sender })
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let line = format!("{}", record.args());
            _ = self
                .line_sender
                .try_send(UIEvent::LogLine(record.level(), line));
        }
    }

    fn flush(&self) {}
}

pub struct UI {
    events: mpsc::Receiver<UIEvent>,
    writer: Option<mpsc::Sender<Message>>,
    connections: Option<ConnectionTable>,
    listeners: HashMap<u16, oneshot::Sender<()>>,
}

impl UI {
    pub fn new(events: mpsc::Receiver<UIEvent>) -> UI {
        UI {
            events,
            writer: None,
            connections: None,
            listeners: HashMap::new(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        enable_raw_mode()?;
        let result = self.run_core().await;
        execute!(stdout(), EnableLineWrap, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        result
    }

    async fn run_core(&mut self) -> Result<()> {
        let mut stdout = stdout();

        execute!(stdout, EnterAlternateScreen, DisableLineWrap)?;
        let mut console_events = EventStream::new();

        let mut connected = false;
        let mut selection = 0;
        let mut show_logs = false;
        let mut lines: VecDeque<String> = VecDeque::with_capacity(1024);
        let mut ports: Option<Vec<PortDesc>> = None;
        loop {
            tokio::select! {
                ev = console_events.next() => {
                    match ev {
                        Some(Ok(Event::Key(ev))) => {
                            match ev {
                                KeyEvent {code:KeyCode::Esc, ..}
                                | KeyEvent {code:KeyCode::Char('q'), ..} => { break; },
                                KeyEvent {code:KeyCode::Char('l'), ..} => {
                                    show_logs = !show_logs;
                                }
                                KeyEvent {code:KeyCode::Char('e'), ..} => {
                                    if let Some(ports) = &ports {
                                        if selection < ports.len() {
                                            let p = ports[selection].port;
                                            self.enable_disable_port(p);
                                        }
                                    }
                                }
                                KeyEvent { code:KeyCode::Up, ..}
                                | KeyEvent { code:KeyCode::Char('j'), ..} => {
                                    if selection > 0 {
                                        selection -= 1;
                                    }
                                }
                                KeyEvent { code:KeyCode::Down, ..}
                                | KeyEvent { code:KeyCode::Char('k'), ..} => {
                                    if let Some(p) = &ports {
                                        if selection != p.len() - 1 {
                                            selection += 1;
                                        }
                                    }
                                }
                                KeyEvent { code:KeyCode::Enter, ..} => {
                                    if let Some(p) = &ports {
                                        if selection < p.len() {
                                            _ = open::that(format!("http://127.0.0.1:{}/", p[selection].port));
                                        }
                                    }
                                }
                                _ => ()
                            }
                        },
                        Some(Ok(_)) => (),  // Don't care about this event...
                        Some(Err(_)) => (), // Hmmmmmm.....?
                        None => (),         // ....no events? what?
                    }
                }
                ev = self.events.recv() => {
                    match ev {
                        Some(UIEvent::Disconnected) => {
                            self.writer = None;
                            self.connections = None;
                            connected = false;
                        }
                        Some(UIEvent::Connected(w,t)) => {
                            self.writer = Some(w);
                            self.connections = Some(t);
                            connected = true;
                        }
                        Some(UIEvent::Ports(mut p)) => {
                            p.sort_by(|a, b| a.port.partial_cmp(&b.port).unwrap());
                            if selection >= p.len() {
                                selection = p.len()-1;
                            }

                            let mut new_listeners = HashMap::new();
                            for port in &p {
                                let port = port.port;
                                if let Some(l) = self.listeners.remove(&port) {
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
                            }

                            // This has the side effect of closing any
                            // listener that didn't get copied over to the
                            // new listeners table.
                            self.listeners = new_listeners;
                            ports = Some(p);
                        }
                        Some(UIEvent::ServerLine(line)) => {
                            while lines.len() >= 1024 {
                                lines.pop_front();
                            }
                            lines.push_back(format!("[SERVER] {line}"));
                        }
                        Some(UIEvent::LogLine(_level, line)) => {
                            while lines.len() >= 1024 {
                                lines.pop_front();
                            }
                            lines.push_back(format!("[CLIENT] {line}"));
                        }
                        None => break,
                    }
                }
            }

            let (columns, rows) = size()?;
            let columns: usize = columns.into();

            queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
            if connected {
                // List of open ports
                // How wide are all the things?
                let padding = 1;
                let enabled_width = 1; // Just 1 character
                let port_width = 5; // 5 characters for 16-bit number

                let description_width =
                    columns - (padding + padding + enabled_width + padding + port_width + padding);

                print!(
                    "{}",
                    format!(
                        "  {enabled:>enabled_width$} {port:>port_width$} {description:<description_width$}\r\n",
                        enabled = "E",
                        port = "port",
                        description = "description"
                    )
                    .negative()
                );
                if let Some(ports) = &mut ports {
                    let max_ports: usize = if show_logs { (rows / 2) - 1 } else { rows - 2 }.into();
                    for (index, port) in ports.into_iter().take(max_ports).enumerate() {
                        print!(
                            "{} {:>enabled_width$} {:port_width$} {:description_width$}\r\n",
                            if index == selection { "\u{2B46}" } else { " " },
                            if self.listeners.contains_key(&port.port) {
                                "+"
                            } else {
                                " "
                            },
                            port.port,
                            port.desc
                        );
                    }
                }
            } else {
                queue!(
                    stdout,
                    PrintStyledContent(
                        format!("{:^columns$}", "Not Connected")
                            .with(Color::Black)
                            .on(Color::Red)
                    )
                )?;
            }

            // Log
            if show_logs {
                let hr: usize = ((rows / 2) - 2).into();
                let start: usize = if lines.len() > hr {
                    lines.len() - hr
                } else {
                    0
                };

                queue!(stdout, MoveTo(0, rows / 2))?;
                print!("{}", format!("{:columns$}", " Log").negative());
                for line in lines.range(start..) {
                    print!("{}\r\n", line);
                }
            }

            queue!(stdout, MoveTo(0, rows - 1))?;
            print!(
                "{}",
                format!(
                    "{:columns$}",
                    " q - quit  |  l - toggle log  |  \u{2191}/\u{2193} - select port  | e - enable/disable | <enter> - browse"
                ).negative()
            );
            stdout.flush()?;
        }

        Ok(())
    }

    fn enable_disable_port(&mut self, port: u16) {
        if let (Some(writer), Some(connections)) = (&self.writer, &self.connections) {
            if let Some(_) = self.listeners.remove(&port) {
                return; // We disabled the listener.
            }

            // We need to enable the listener.
            let (l, stop) = oneshot::channel();
            self.listeners.insert(port, l);

            let (writer, connections) = (writer.clone(), connections.clone());
            tokio::spawn(async move {
                let result = tokio::select! {
                    r = client_listen(port, writer, connections) => r,
                    _ = stop => Ok(()),
                };
                if let Err(e) = result {
                    error!("Error listening on port {port}: {e:?}");
                } else {
                    info!("Stopped listening on port {port}");
                }
            });
        }
    }
}
