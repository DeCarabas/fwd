use crate::message::PortDesc;
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{
        Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use std::io::stdout;
use tokio::sync::mpsc;

pub async fn run_ui(port_receiver: &mut mpsc::Receiver<Vec<PortDesc>>) -> Result<()> {
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, DisableLineWrap)?;
    while let Some(mut ports) = port_receiver.recv().await {
        ports.sort_by(|a, b| a.port.partial_cmp(&b.port).unwrap());

        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        println!("Port   Url                      Description");
        println!("-----  ------------------------ -----------");
        for port in ports {
            println!(
                "{:5} {:24} {}",
                port.port,
                format!("http://locahost:{}/", port.port),
                port.desc
            );
        }
    }
    execute!(stdout, EnableLineWrap, LeaveAlternateScreen)?;
    Ok(())
}
