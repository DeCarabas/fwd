use crate::message::PortDesc;
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
use log::{Level, Metadata, Record};
use open;
use std::collections::vec_deque::VecDeque;
use std::io::{stdout, Write};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

pub enum UIEvent {
    Connected,
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

pub async fn run_ui(events: &mut mpsc::Receiver<UIEvent>) -> Result<()> {
    enable_raw_mode()?;
    let result = run_ui_core(events).await;
    execute!(stdout(), EnableLineWrap, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    result
}

async fn run_ui_core(events: &mut mpsc::Receiver<UIEvent>) -> Result<()> {
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
            ev = events.recv() => {
                match ev {
                    Some(UIEvent::Disconnected) => {
                        connected = false;
                    }
                    Some(UIEvent::Connected) => {
                        connected = true;
                    }
                    Some(UIEvent::Ports(mut p)) => {
                        p.sort_by(|a, b| a.port.partial_cmp(&b.port).unwrap());
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
            let port_width = 5; // 5 characters for 16-bit number

            let description_width = columns - (padding + padding + port_width + padding);

            print!(
                "{}",
                format!(
                    "  {port:>port_width$} {description:<description_width$}\r\n",
                    port = "port",
                    description = "description"
                )
                .negative()
            );
            if let Some(ports) = &mut ports {
                let max_ports: usize = if show_logs { (rows / 2) - 1 } else { rows - 2 }.into();
                for (index, port) in ports.into_iter().take(max_ports).enumerate() {
                    print!(
                        "{} {:port_width$} {:description_width$}\r\n",
                        if index == selection { "\u{2B46}" } else { " " },
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
                " q - quit  |  l - toggle log  |  \u{2191}/\u{2193} - select port  | <enter> - browse"
            )
            .negative()
        );
        stdout.flush()?;
    }

    Ok(())
}
