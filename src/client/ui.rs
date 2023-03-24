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
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row,
        Table, TableState,
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
    show_help: bool,
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
            show_help: false,
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
        if self.show_help {
            self.render_help(frame);
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
                    if listener.enabled { " ✓ " } else { "" },
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
        let widths = vec![
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(size.width),
        ];

        let port_list = Table::new(rows)
            .header(Row::new(vec!["fwd", "Port", "Description"]))
            .block(Block::default().title("Ports").borders(Borders::ALL))
            .column_spacing(1)
            .widths(&widths)
            .highlight_symbol(">> ");

        frame.render_stateful_widget(port_list, size, &mut self.selection);
    }

    fn render_help<B: Backend>(&mut self, frame: &mut Frame<B>) {
        let keybindings = vec![
            Row::new(vec!["↑ / k", "Move cursor up"]),
            Row::new(vec!["↓ / j", "Move cursor down"]),
            Row::new(vec!["e", "Enable/disable forwarding"]),
            Row::new(vec!["RET", "Open port in web browser"]),
            Row::new(vec!["ESC / q", "Quit"]),
            Row::new(vec!["? / h", "Show this help text"]),
            Row::new(vec!["l", "Show fwd's logs"]),
        ];

        let help_intro = 4;
        let border_lines = 3;

        let help_popup_area = centered_rect(
            65,
            keybindings.len() as u16 + help_intro + border_lines,
            frame.size(),
        );
        let inner_area =
            help_popup_area.inner(&Margin { vertical: 1, horizontal: 1 });

        let constraints = vec![
            Constraint::Length(help_intro),
            Constraint::Length(inner_area.height - help_intro),
        ];
        let help_parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner_area);

        let keybindings = Table::new(keybindings)
            .widths(&[Constraint::Length(7), Constraint::Length(40)])
            .column_spacing(1)
            .block(Block::default().title("Keys").borders(Borders::TOP));

        let exp = Paragraph::new(
            "fwd automatically listens for connections on the same ports\nas the target, and forwards connections on those ports to the\ntarget.",
        );

        // outer box
        frame.render_widget(Clear, help_popup_area); //this clears out the background
        let helpbox = Block::default().title("Help").borders(Borders::ALL);
        frame.render_widget(helpbox, help_popup_area);

        // explanation
        frame.render_widget(exp, help_parts[0]);

        // keybindings
        frame.render_widget(keybindings, help_parts[1]);
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
        if self.show_help {
            self.handle_console_event_help(ev)
        } else {
            self.handle_console_event_main(ev)
        }
    }

    fn handle_console_event_help(
        &mut self,
        ev: Option<Result<Event, std::io::Error>>,
    ) {
        match ev {
            Some(Ok(Event::Key(ev))) => match ev {
                KeyEvent { code: KeyCode::Esc, .. }
                | KeyEvent { code: KeyCode::Char('q'), .. }
                | KeyEvent { code: KeyCode::Char('?'), .. }
                | KeyEvent { code: KeyCode::Char('h'), .. } => {
                    self.show_help = false;
                }
                _ => (),
            },
            Some(Ok(_)) => (), // Don't care about this event...
            Some(Err(_)) => (), // Hmmmmmm.....?
            None => (),        // ....no events? what?
        }
    }

    fn handle_console_event_main(
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
                KeyEvent { code: KeyCode::Char('?'), .. }
                | KeyEvent { code: KeyCode::Char('h'), .. } => {
                    self.show_help = !self.show_help;
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

                // Grab the selected port
                let selected_port = self.get_selected_port();

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

                let selected = match selected_port {
                    Some(port) => {
                        match self.get_ui_ports().binary_search(&port) {
                            Ok(index) => Some(index),
                            Err(_) => None,
                        }
                    }
                    None => None,
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

/// helper function to create a centered rect using up certain percentage of the available rect `r`
fn centered_rect(width_chars: u16, height_chars: u16, r: Rect) -> Rect {
    let left = r.left()
        + if width_chars > r.width {
            0
        } else {
            (r.width - width_chars) / 2
        };

    let top = r.top()
        + if height_chars > r.height {
            0
        } else {
            (r.height - height_chars) / 2
        };

    let width = width_chars.min(r.width);
    let height = height_chars.min(r.height);

    Rect::new(left, top, width, height)
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

        // ...but now the port I had selected goes away, my selection should clear.
        ui.handle_internal_event(Some(UIEvent::Ports(vec![PortDesc {
            port: 8080,
            desc: "my-service".to_string(),
        }])));
        assert_eq!(ui.ports.len(), 1);
        assert_matches!(ui.selection.selected(), None);

        // Put it back but we don't re-select it. (Sorry bro?)
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
        assert_matches!(ui.selection.selected(), None);

        // Now, there are ports...
        ui.handle_internal_event(Some(UIEvent::Ports(vec![
            PortDesc {
                port: 8080,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8081,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8082,
                desc: "my-service".to_string(),
            },
        ])));

        // Select the middle one.
        ui.selection.select(Some(1));

        // Drop the first one, the selection should move up.
        ui.handle_internal_event(Some(UIEvent::Ports(vec![
            PortDesc {
                port: 8081,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8082,
                desc: "my-service".to_string(),
            },
        ])));
        assert_matches!(ui.selection.selected(), Some(0));

        // Insert two at the top, it should move down.
        ui.handle_internal_event(Some(UIEvent::Ports(vec![
            PortDesc {
                port: 8079,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8080,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8081,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8082,
                desc: "my-service".to_string(),
            },
        ])));
        assert_matches!(ui.selection.selected(), Some(2));

        drop(sender);
    }

    #[test]
    fn port_selection_wrapping() {
        let (sender, receiver) = mpsc::channel(64);
        let config = ServerConfig::default();
        let mut ui = UI::new(receiver, config);

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
        assert_matches!(ui.selection.selected(), None);

        // Move selection up => wraps around the length
        ui.selection.select(Some(0));
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::empty(),
        )))));
        assert_matches!(ui.selection.selected(), Some(1));
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::empty(),
        )))));
        assert_matches!(ui.selection.selected(), Some(0));
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::empty(),
        )))));
        assert_matches!(ui.selection.selected(), Some(1));

        // Move selection down => wraps around the length
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::empty(),
        )))));
        assert_matches!(ui.selection.selected(), Some(0));
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::empty(),
        )))));
        assert_matches!(ui.selection.selected(), Some(1));
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::empty(),
        )))));
        assert_matches!(ui.selection.selected(), Some(0));

        // J and K move the correct direction
        ui.handle_internal_event(Some(UIEvent::Ports(vec![
            PortDesc {
                port: 8080,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8081,
                desc: "my-service".to_string(),
            },
            PortDesc {
                port: 8082,
                desc: "my-service".to_string(),
            },
        ])));
        assert_eq!(ui.ports.len(), 3);

        // J is down
        ui.selection.select(Some(1));
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::empty(),
        )))));
        assert_matches!(ui.selection.selected(), Some(2));

        // K is up
        ui.selection.select(Some(1));
        ui.handle_console_event(Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::empty(),
        )))));
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

    #[test]
    fn test_centered_rect() {
        // Normal old centering.
        let frame = Rect::new(0, 0, 128, 128);
        let centered = centered_rect(10, 10, frame);
        assert_eq!(centered.left(), (128 - centered.width) / 2);
        assert_eq!(centered.top(), (128 - centered.height) / 2);
        assert_eq!(centered.width, 10);
        assert_eq!(centered.height, 10);

        // Clip the width and height to the box
        let frame = Rect::new(0, 0, 5, 5);
        let centered = centered_rect(10, 10, frame);
        assert_eq!(centered.left(), 0);
        assert_eq!(centered.top(), 0);
        assert_eq!(centered.width, 5);
        assert_eq!(centered.height, 5);

        // Deal with non zero-zero origins.
        let frame = Rect::new(10, 10, 128, 128);
        let centered = centered_rect(10, 10, frame);
        assert_eq!(centered.left(), 10 + (128 - centered.width) / 2);
        assert_eq!(centered.top(), 10 + (128 - centered.height) / 2);
        assert_eq!(centered.width, 10);
        assert_eq!(centered.height, 10);
    }
}
