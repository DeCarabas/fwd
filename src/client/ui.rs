use super::client_listen;
use crate::message::PortDesc;
use anyhow::Result;
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
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
use std::collections::{HashMap, HashSet};
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
struct Listener {
    enabled: bool,
    stop: Option<oneshot::Sender<()>>,
    desc: Option<PortDesc>,
}

impl Listener {
    pub fn from_desc(desc: PortDesc) -> Listener {
        Listener {
            enabled: false,
            stop: None,
            desc: Some(desc),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, socks_port: Option<u16>, enabled: bool) {
        if enabled {
            self.enabled = true;
            self.start(socks_port);
        } else {
            self.enabled = false;
            self.stop = None;
        }
    }

    pub fn connect(&mut self, socks_port: Option<u16>, desc: PortDesc) {
        self.desc = Some(desc);
        self.start(socks_port);
    }

    pub fn disconnect(&mut self) {
        self.desc = None;
        self.stop = None;
    }

    pub fn start(&mut self, socks_port: Option<u16>) {
        if self.enabled {
            if let (Some(desc), Some(socks_port), None) =
                (&self.desc, socks_port, &self.stop)
            {
                let (l, stop) = oneshot::channel();
                let port = desc.port;
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

                self.stop = Some(l);
            }
        }
    }
}

#[derive(Debug)]
pub struct UI {
    events: mpsc::Receiver<UIEvent>,
    ports: HashMap<u16, Listener>,
    socks_port: Option<u16>,
    running: bool,
    show_logs: bool,
    selection: usize,
    lines: VecDeque<String>,
    alternate_screen: bool,
    raw_mode: bool,
}

impl UI {
    pub fn new(events: mpsc::Receiver<UIEvent>) -> UI {
        UI {
            events,
            ports: HashMap::new(),
            socks_port: None,
            running: true,
            show_logs: false,
            selection: 0,
            lines: VecDeque::with_capacity(1024),
            alternate_screen: false,
            raw_mode: false,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
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
        self.enter_alternate_screen()?;
        self.disable_raw_mode()?;
        let mut stdout = stdout();

        let (columns, _) = size()?;
        let columns: usize = columns.into();

        execute!(
            stdout,
            SavePosition,
            MoveTo(0, 0),
            PrintStyledContent(
                format!("{:^columns$}\r\n", "Not Connected")
                    .with(Color::Black)
                    .on(Color::Red)
            ),
            RestorePosition,
        )?;
        Ok(())
    }

    fn render_connected(&mut self) -> Result<()> {
        self.enter_alternate_screen()?;
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

        let max_ports: usize = if self.show_logs {
            (rows / 2) - 1
        } else {
            rows - 2
        }
        .into();

        let ports = self.get_ui_ports();
        for (index, port) in ports.into_iter().take(max_ports).enumerate() {
            let listener = self.ports.get(&port).unwrap();

            let caret = if index == self.selection {
                "\u{2B46}"
            } else {
                " "
            };

            let state = if listener.enabled { "+" } else { " " };
            let desc: &str = match &listener.desc {
                Some(port_desc) => &port_desc.desc,
                None => "",
            };

            print!("{caret} {state:>enabled_width$} {port:port_width$} {desc:description_width$}\r\n");
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
        match self.socks_port {
            Some(_) => true,
            None => false,
        }
    }

    fn get_ui_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self.ports.keys().copied().collect();
        ports.sort();
        ports
    }

    fn get_selected_port(&self) -> Option<u16> {
        if self.selection < self.ports.len() {
            Some(self.get_ui_ports()[self.selection])
        } else {
            None
        }
    }

    fn enable_disable_port(&mut self, port: u16) {
        if let Some(listener) = self.ports.get_mut(&port) {
            listener.set_enabled(self.socks_port, !listener.enabled());
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
                        self.enable_disable_port(p);
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
                    if self.selection != self.ports.len() - 1 {
                        self.selection += 1;
                    }
                }
                KeyEvent { code: KeyCode::Enter, .. } => {
                    if let Some(p) = self.get_selected_port() {
                        _ = open::that(format!("http://127.0.0.1:{}/", p));
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
                self.socks_port = None;
                for port in self.ports.values_mut() {
                    port.disconnect();
                }
            }
            Some(UIEvent::Connected(sp)) => {
                info!("Socks port {sp}");
                self.socks_port = Some(sp);
                for port in self.ports.values_mut() {
                    port.start(self.socks_port);
                }
            }
            Some(UIEvent::Ports(p)) => {
                let mut leftover_ports: HashSet<u16> =
                    HashSet::from_iter(self.ports.keys().copied());

                for port_desc in p.into_iter() {
                    leftover_ports.remove(&port_desc.port);
                    if let Some(listener) = self.ports.get_mut(&port_desc.port)
                    {
                        listener.connect(self.socks_port, port_desc);
                    } else {
                        self.ports.insert(
                            port_desc.port,
                            Listener::from_desc(port_desc),
                        );
                    }
                }

                for port in leftover_ports {
                    let mut enabled = false;
                    if let Some(listener) = self.ports.get_mut(&port) {
                        enabled = listener.enabled;
                        listener.disconnect();
                    }

                    if !enabled {
                        // Just... forget it? Or leave it around forever?
                        self.ports.remove(&port);
                    }
                }

                if self.selection >= self.ports.len() && self.ports.len() > 0 {
                    self.selection = self.ports.len() - 1;
                }
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

impl Drop for UI {
    fn drop(&mut self) {
        _ = self.disable_raw_mode();
        _ = self.leave_alternate_screen();
    }
}
