use crate::client_listen;
use crate::message::PortDesc;
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Color, PrintStyledContent, Stylize},
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType,
        DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen,
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
    Connected(u16),
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

#[derive(Debug)]
pub struct UI {
    events: mpsc::Receiver<UIEvent>,
    listeners: HashMap<u16, oneshot::Sender<()>>,
    socks_port: u16,
    running: bool,
    show_logs: bool,
    selection: usize,
    ports: Option<Vec<PortDesc>>,
    lines: VecDeque<String>,
    alternate_screen: bool,
    raw_mode: bool,
}

impl UI {
    pub fn new(events: mpsc::Receiver<UIEvent>) -> UI {
        UI {
            events,
            listeners: HashMap::new(),
            socks_port: 0,
            running: true,
            show_logs: false,
            selection: 0,
            ports: None,
            lines: VecDeque::with_capacity(1024),
            alternate_screen: false,
            raw_mode: false,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        self.enter_alternate_screen()?;
        let result = self.run_core().await;
        _ = self.disable_raw_mode();
        _ = self.leave_alternate_screen();
        result
    }

    async fn run_core(&mut self) -> Result<()> {
        let mut console_events = EventStream::new();

        self.running = true;
        while self.running {
            self.handle_events(&mut console_events).await;

            if self.connected() {
                self.render_connected()?;
            } else {
                self.render_disconnected()?;
            }
        }

        Ok(())
    }

    fn render_disconnected(&mut self) -> Result<()> {
        self.disable_raw_mode()?;
        let mut stdout = stdout();

        let (columns, _) = size()?;
        let columns: usize = columns.into();

        execute!(
            stdout,
            Clear(ClearType::All),
            MoveTo(0, 0),
            PrintStyledContent(
                format!("{:^columns$}\r\n", "Not Connected")
                    .with(Color::Black)
                    .on(Color::Red)
            ),
        )?;
        Ok(())
    }

    fn render_connected(&mut self) -> Result<()> {
        self.enable_raw_mode()?;
        let mut stdout = stdout();

        let (columns, rows) = size()?;
        let columns: usize = columns.into();

        queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

        // List of open ports
        // How wide are all the things?
        let padding = 1;
        let enabled_width = 1; // Just 1 character
        let port_width = 5; // 5 characters for 16-bit number

        let description_width = columns
            - (padding
                + padding
                + enabled_width
                + padding
                + port_width
                + padding);

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
        if let Some(ports) = &self.ports {
            let max_ports: usize = if self.show_logs {
                (rows / 2) - 1
            } else {
                rows - 2
            }
            .into();
            for (index, port) in ports.into_iter().take(max_ports).enumerate() {
                print!(
                            "{} {:>enabled_width$} {:port_width$} {:description_width$}\r\n",
                            if index == self.selection {
                                "\u{2B46}"
                            } else {
                                " "
                            },
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

        // Log
        if self.show_logs {
            let hr: usize = ((rows / 2) - 2).into();
            let start: usize = if self.lines.len() > hr {
                self.lines.len() - hr
            } else {
                0
            };

            queue!(stdout, MoveTo(0, rows / 2))?;
            print!("{}", format!("{:columns$}", " Log").negative());
            for line in self.lines.range(start..) {
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
        Ok(())
    }

    fn connected(&self) -> bool {
        self.socks_port != 0
    }

    fn get_selected_port(&self) -> Option<&PortDesc> {
        if let Some(p) = &self.ports {
            if self.selection < p.len() {
                return Some(&p[self.selection]);
            }
        }
        None
    }

    fn enable_disable_port(&mut self, port: u16) {
        if self.socks_port != 0 {
            if let Some(_) = self.listeners.remove(&port) {
                return; // We disabled the listener.
            }

            // We need to enable the listener.
            let (l, stop) = oneshot::channel();
            self.listeners.insert(port, l);

            let socks_port = self.socks_port;
            tokio::spawn(async move {
                let result = tokio::select! {
                    r = client_listen(port, socks_port) => r,
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

    fn enable_raw_mode(&mut self) -> Result<()> {
        if !self.raw_mode {
            enable_raw_mode()?;
            self.raw_mode = true;
        }
        Ok(())
    }

    fn disable_raw_mode(&mut self) -> Result<()> {
        if self.raw_mode {
            disable_raw_mode()?;
            self.raw_mode = false;
        }
        Ok(())
    }

    fn enter_alternate_screen(&mut self) -> Result<()> {
        if !self.alternate_screen {
            enable_raw_mode()?;
            execute!(stdout(), EnterAlternateScreen, DisableLineWrap)?;
            self.alternate_screen = true;
        }
        Ok(())
    }

    fn leave_alternate_screen(&mut self) -> Result<()> {
        if self.alternate_screen {
            execute!(stdout(), LeaveAlternateScreen, EnableLineWrap)?;
            disable_raw_mode()?;
            self.alternate_screen = false;
        }
        Ok(())
    }

    async fn handle_events(&mut self, console_events: &mut EventStream) {
        tokio::select! {
            ev = console_events.next() => self.handle_console_event(ev),
            ev = self.events.recv() => self.handle_internal_event(ev),
        }
    }

    fn handle_console_event(
        &mut self,
        ev: Option<Result<Event, std::io::Error>>,
    ) {
        match ev {
            Some(Ok(Event::Key(ev))) => match ev {
                KeyEvent { code: KeyCode::Char('c'), .. } => {
                    if ev.modifiers.intersects(KeyModifiers::CONTROL) {
                        self.running = false;
                    }
                }
                KeyEvent { code: KeyCode::Esc, .. }
                | KeyEvent { code: KeyCode::Char('q'), .. } => {
                    self.running = false;
                }
                KeyEvent { code: KeyCode::Char('l'), .. } => {
                    self.show_logs = !self.show_logs;
                }
                KeyEvent { code: KeyCode::Char('e'), .. } => {
                    if let Some(p) = self.get_selected_port() {
                        self.enable_disable_port(p.port);
                    }
                }
                KeyEvent { code: KeyCode::Up, .. }
                | KeyEvent { code: KeyCode::Char('j'), .. } => {
                    if self.selection > 0 {
                        self.selection -= 1;
                    }
                }
                KeyEvent { code: KeyCode::Down, .. }
                | KeyEvent { code: KeyCode::Char('k'), .. } => {
                    if let Some(p) = &self.ports {
                        if self.selection != p.len() - 1 {
                            self.selection += 1;
                        }
                    }
                }
                KeyEvent { code: KeyCode::Enter, .. } => {
                    if let Some(p) = self.get_selected_port() {
                        _ = open::that(format!("http://127.0.0.1:{}/", p.port));
                    }
                }
                _ => (),
            },
            Some(Ok(_)) => (), // Don't care about this event...
            Some(Err(_)) => (), // Hmmmmmm.....?
            None => (),        // ....no events? what?
        }
    }

    fn handle_internal_event(&mut self, event: Option<UIEvent>) {
        match event {
            Some(UIEvent::Disconnected) => {
                self.socks_port = 0;
                self.listeners = HashMap::new(); // Bye.
            }
            Some(UIEvent::Connected(sp)) => {
                self.socks_port = sp;
                info!("Socks port {socks_port}", socks_port = self.socks_port);
            }
            Some(UIEvent::Ports(mut p)) => {
                p.sort_by(|a, b| a.port.partial_cmp(&b.port).unwrap());
                if self.selection >= p.len() && p.len() > 0 {
                    self.selection = p.len() - 1;
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
                self.ports = Some(p);
            }
            Some(UIEvent::ServerLine(line)) => {
                while self.lines.len() >= 1024 {
                    self.lines.pop_front();
                }
                self.lines.push_back(format!("[SERVER] {line}"));
            }
            Some(UIEvent::LogLine(_level, line)) => {
                while self.lines.len() >= 1024 {
                    self.lines.pop_front();
                }
                self.lines.push_back(format!("[CLIENT] {line}"));
            }
            None => {
                self.running = false;
            }
        }
    }
}
