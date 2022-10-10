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
use std::io::{stdout, Write};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

pub async fn run_ui(port_receiver: &mut mpsc::Receiver<Vec<PortDesc>>) -> Result<()> {
    enable_raw_mode()?;
    let result = run_ui_core(port_receiver).await;
    execute!(stdout(), EnableLineWrap, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    result
}

async fn run_ui_core(port_receiver: &mut mpsc::Receiver<Vec<PortDesc>>) -> Result<()> {
    let mut stdout = stdout();

    execute!(stdout, EnterAlternateScreen, DisableLineWrap)?;
    let mut events = EventStream::new();

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
        }

        let (columns, rows) = size()?;

        queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;

        // How wide are all the things?
        let columns: usize = columns.into();
        let padding = 1;
        let port_width = 5; // 5 characters for 16-bit number
        let url_width = "http://localhost:/".len() + port_width;

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
            for port in ports {
                print!(
                    " {:port_width$} {:url_width$} {:description_width$}\r\n",
                    port.port,
                    format!("http://locahost:{}/", port.port),
                    port.desc
                );
            }
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
