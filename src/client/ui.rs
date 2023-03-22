use super::{client_listen, config::ServerConfig};
use crate::message::PortDesc;
use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, DisableLineWrap, EnableLineWrap,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use log::{error, info, Level, Metadata, Record};
use open;
use std::collections::vec_deque::VecDeque;
use std::collections::{HashMap, HashSet};
use std::io::stdout;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{
        Block, Borders, List, ListItem, ListState, Row, Table, TableState,
    },
    Frame, Terminal,
};

pub enum UIEvent {
    Connected(u16),
    Disconnected,
    ServerLine(String),
    LogLine(log::Level, String),
    Ports(Vec<PortDesc>),
}

pub enum UIReturn {
    Quit,
    Disconnected,
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
    pub fn from_desc(
        socks_port: Option<u16>,
        desc: PortDesc,
        enabled: bool,
    ) -> Listener {
        let mut listener = Listener { enabled, stop: None, desc: Some(desc) };
        if enabled {
            listener.start(socks_port);
        }
        listener
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
                info!("Starting port {port} to {socks_port}", port = desc.port);
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
    lines: VecDeque<String>,
    config: ServerConfig,
    selection: TableState,
    running: bool,
    show_logs: bool,
    alternate_screen: bool,
    raw_mode: bool,
}

impl UI {
    pub fn new(events: mpsc::Receiver<UIEvent>, config: ServerConfig) -> UI {
        UI {
            events,
            ports: HashMap::new(),
            socks_port: None,
            running: true,
            show_logs: false,
            selection: TableState::default(),
            lines: VecDeque::with_capacity(1024),
            config,
            alternate_screen: false,
            raw_mode: false,
        }
    }

    pub async fn run(&mut self) -> Result<UIReturn> {
        loop {
            while !self.connected() {
                let ev = self.events.recv().await;
                self.handle_internal_event(ev);
            }

            let result = self.run_connected().await;
            _ = self.leave_alternate_screen();
            _ = self.disable_raw_mode();

            match result {
                Err(e) => {
                    return Err(e);
                }
                Ok(UIReturn::Disconnected) => {
                    continue;
                }
                Ok(UIReturn::Quit) => {
                    return Ok(UIReturn::Quit);
                }
            }
        }
    }

    async fn run_connected(&mut self) -> Result<UIReturn> {
        let mut console_events = EventStream::new();
        self.enter_alternate_screen()?;
        self.enable_raw_mode()?;

        let stdout = std::io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        self.running = true;
        while self.running && self.connected() {
            self.handle_events(&mut console_events).await;
            terminal.draw(|f| {
                self.render_connected(f);
            })?;
        }

        let code = if self.running {
            UIReturn::Disconnected
        } else {
            UIReturn::Quit
        };
        Ok(code)
    }

    fn render_connected<T: Backend>(&mut self, frame: &mut Frame<T>) {
        let constraints = if self.show_logs {
            vec![Constraint::Percentage(50), Constraint::Percentage(50)]
        } else {
            vec![Constraint::Percentage(100)]
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.size());

        self.render_ports(frame, chunks[0]);
        if self.show_logs {
            self.render_logs(frame, chunks[1]);
        }
    }

    fn render_ports<B: Backend>(&mut self, frame: &mut Frame<B>, size: Rect) {
        let enabled_port_style = Style::default();
        let disabled_port_style = Style::default().fg(Color::DarkGray);

        let mut rows = Vec::new();
        let ports = self.get_ui_ports();
        let port_strings: Vec<_> =
            ports.iter().map(|p| format!("{p}")).collect();
        for (index, port) in ports.into_iter().enumerate() {
            let listener = self.ports.get(&port).unwrap();
            rows.push(
                Row::new(vec![
                    &port_strings[index][..],
                    match &listener.desc {
                        Some(port_desc) => &port_desc.desc,
                        None => "",
                    },
                ])
                .style(if listener.enabled {
                    enabled_port_style
                } else {
                    disabled_port_style
                }),
            );
        }

        // TODO: I don't know how to express the lengths I want here.
        //       That last length is extremely wrong but guaranteed to work I
        //       guess.
        let widths =
            vec![Constraint::Length(5), Constraint::Length(size.width)];

        let port_list = Table::new(rows)
            .header(Row::new(vec!["Port", "Description"]))
            .block(Block::default().title("Ports").borders(Borders::ALL))
            .column_spacing(1)
            .widths(&widths)
            .highlight_symbol(">> ");

        frame.render_stateful_widget(port_list, size, &mut self.selection);
    }

    fn render_logs<B: Backend>(&mut self, frame: &mut Frame<B>, size: Rect) {
        let items: Vec<_> =
            self.lines.iter().map(|l| ListItem::new(&l[..])).collect();

        let list = List::new(items)
            .block(Block::default().title("Log").borders(Borders::ALL));

        let mut list_state = ListState::default();
        list_state.select(if self.lines.len() > 0 {
            Some(self.lines.len() - 1)
        } else {
            None
        });

        frame.render_stateful_widget(list, size, &mut list_state);
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
        self.selection.selected().map(|i| self.get_ui_ports()[i])
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
                | KeyEvent { code: KeyCode::Char('k'), .. } => {
                    let index = match self.selection.selected() {
                        Some(i) => {
                            assert!(self.ports.len() > 0, "We must have ports because we have a selection.");
                            if i == 0 {
                                self.ports.len() - 1
                            } else {
                                i - 1
                            }
                        }
                        None => 0,
                    };
                    self.selection.select(Some(index));
                }
                KeyEvent { code: KeyCode::Down, .. }
                | KeyEvent { code: KeyCode::Char('j'), .. } => {
                    let index = match self.selection.selected() {
                        Some(i) => {
                            assert!(self.ports.len() > 0, "We must have ports because we have a selection.");
                            (i + 1) % self.ports.len()
                        }
                        None => 0,
                    };
                    self.selection.select(Some(index));
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
                        let config = self.config.get(port_desc.port);
                        info!("Port config {port_desc:?} -> {config:?}");

                        self.ports.insert(
                            port_desc.port,
                            Listener::from_desc(
                                self.socks_port,
                                port_desc,
                                config.enabled,
                            ),
                        );
                    }
                }

                for port in leftover_ports {
                    if let Some(listener) = self.ports.get_mut(&port) {
                        listener.disconnect();
                    }

                    if !self.config.contains_key(port) {
                        self.ports.remove(&port);
                    }
                }

                let selected = if self.ports.len() == 0 {
                    None // No ports, no selection.
                } else {
                    match self.selection.selected() {
                        Some(i) => Some(i.min(self.ports.len() - 1)),
                        None => Some(0),
                    }
                };
                self.selection.select(selected);
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

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn empty_ports() {
        let (sender, receiver) = mpsc::channel(64);
        let config = ServerConfig::default();
        let mut ui = UI::new(receiver, config);

        // There are ports...
        ui.handle_internal_event(Some(UIEvent::Ports(vec![PortDesc {
            port: 8080,
            desc: "my-service".to_string(),
        }])));
        ui.selection.select(Some(0));

        // ...but now there are no ports!
        ui.handle_internal_event(Some(UIEvent::Ports(vec![])));
        assert_eq!(ui.ports.len(), 0);
        assert_matches!(ui.selection.selected(), None);

        drop(sender);
    }

    #[test]
    fn port_change_selection() {
        let (sender, receiver) = mpsc::channel(64);
        let config = ServerConfig::default();
        let mut ui = UI::new(receiver, config);

        // There are ports...
        ui.handle_internal_event(Some(UIEvent::Ports(vec![
            PortDesc {
                port: 8080,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8081,
                desc: "my-service".to_string(),
            },
        ])));
        ui.selection.select(Some(1));

        // ...but now there is one fewer port, selection should move.
        ui.handle_internal_event(Some(UIEvent::Ports(vec![PortDesc {
            port: 8080,
            desc: "my-service".to_string(),
        }])));
        assert_eq!(ui.ports.len(), 1);
        assert_matches!(ui.selection.selected(), Some(0));

        // Put it back but selection doesn't move
        ui.handle_internal_event(Some(UIEvent::Ports(vec![
            PortDesc {
                port: 8080,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8081,
                desc: "my-service".to_string(),
            },
        ])));
        assert_eq!(ui.ports.len(), 2);
        assert_matches!(ui.selection.selected(), Some(0));

        drop(sender);
    }

    #[test]
    fn log_lines() {
        let (sender, receiver) = mpsc::channel(64);
        let config = ServerConfig::default();
        let mut ui = UI::new(receiver, config);

        // Client and server are all formatted right you know.
        ui.handle_internal_event(Some(UIEvent::ServerLine("A".to_string())));
        ui.handle_internal_event(Some(UIEvent::LogLine(
            Level::Info,
            "A".to_string(),
        )));
        assert_eq!(ui.lines.len(), 2);
        assert_eq!(ui.lines[0], "[SERVER] A".to_string());
        assert_eq!(ui.lines[1], "[CLIENT] A".to_string());

        // Make sure we bound the log.
        for _ in 1..2048 {
            ui.handle_internal_event(Some(UIEvent::ServerLine(
                "A".to_string(),
            )));
        }
        assert_eq!(ui.lines.len(), 1024);

        drop(sender);
    }
}
