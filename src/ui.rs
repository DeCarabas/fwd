use crate::message::PortDesc;
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{Event, EventStream, KeyCode, KeyEvent},
    execute, queue,
    style::Stylize,
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType, DisableLineWrap, EnableLineWrap,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use log::{Level, Metadata, Record};
use std::collections::vec_deque::VecDeque;
use std::io::{stdout, Write};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

#[derive(Debug, Clone)]
pub struct Logger {
    line_sender: mpsc::Sender<String>,
}

impl Logger {
    pub fn new(line_sender: mpsc::Sender<String>) -> Box<Logger> {
        Box::new(Logger { line_sender })
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let line = format!("{} - {}", record.level(), record.args());
            _ = self.line_sender.try_send(line);
        }
    }

    fn flush(&self) {}
}

pub async fn run_ui(
    port_receiver: &mut mpsc::Receiver<Vec<PortDesc>>,
    log_receiver: &mut mpsc::Receiver<String>,
) -> Result<()> {
    enable_raw_mode()?;
    let result = run_ui_core(port_receiver, log_receiver).await;
    execute!(stdout(), EnableLineWrap, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    result
}

async fn run_ui_core(
    port_receiver: &mut mpsc::Receiver<Vec<PortDesc>>,
    log: &mut mpsc::Receiver<String>,
) -> Result<()> {
    let mut stdout = stdout();

    execute!(stdout, EnterAlternateScreen, DisableLineWrap)?;
    let mut events = EventStream::new();

    let mut lines: VecDeque<String> = VecDeque::with_capacity(1024);
    let mut ports = None;
    loop {
        tokio::select! {
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(ev))) => {
                        match ev {
                            KeyEvent {code:KeyCode::Esc, ..} => { break; },
                            KeyEvent {code:KeyCode::Char('q'), ..} => { break; },
                            _ => ()
                        }
                    },
                    Some(Ok(_)) => (),  // Don't care about this event...
                    Some(Err(_)) => (), // Hmmmmmm.....?
                    None => (),         // ....no events? what?
                }
            }
            pr = port_receiver.recv() => {
                match pr {
                    Some(p) => { ports = Some(p); }
                    None => break,
                }
            }
            l = log.recv() => {
                match l {
                    Some(line) => {
                        if lines.len() > 1024 {
                            lines.pop_front();
                        }
                        lines.push_back(line);
                    },
                    None => break,
                }
            }
        }

        let (columns, rows) = size()?;

        queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

        // How wide are all the things?
        let columns: usize = columns.into();
        let padding = 1;
        let port_width = 5; // 5 characters for 16-bit number
        let url_width = "http://127.0.0.1:/".len() + port_width;

        let description_width = columns - (padding + port_width + padding + url_width + padding);

        print!(
            "{}",
            format!(
                " {port:>port_width$} {url:<url_width$} {description:<description_width$}\r\n",
                port = "port",
                url = "url",
                description = "description"
            )
            .negative()
        );
        if let Some(ports) = &mut ports {
            ports.sort_by(|a, b| a.port.partial_cmp(&b.port).unwrap());
            for port in ports.into_iter().take(((rows / 2) - 1).into()) {
                print!(
                    " {:port_width$} {:url_width$} {:description_width$}\r\n",
                    port.port,
                    format!("http://127.0.0.1:{}/", port.port),
                    port.desc
                );
            }
        }

        let hr: usize = ((rows / 2) - 1).into();
        let start: usize = if lines.len() > hr {
            lines.len() - hr
        } else {
            0
        };

        queue!(stdout, MoveTo(0, rows / 2))?;
        for line in lines.range(start..) {
            print!("{}\r\n", line);
        }

        queue!(stdout, MoveTo(0, rows - 1))?;
        print!(
            "{}",
            format!("{:columns$}", " Press ESC or q to quit").negative()
        );
        stdout.flush()?;
    }

    Ok(())
}
