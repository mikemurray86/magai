mod ai;
mod approval;
mod memory;
mod config;
mod hooks;
mod mcp;
mod plugins;
mod skills;
mod slash_commands;
mod tools;
mod ui;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use tokio::sync::mpsc;
use ui::AiEvent;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (user_tx, user_rx) = mpsc::unbounded_channel::<ai::AgentCommand>();
    let (ai_tx, ai_rx) = mpsc::unbounded_channel::<AiEvent>();

    let (cfg, config_warning) = config::Config::load();
    if let Some(warning) = config_warning {
        ai_tx
            .send(AiEvent::Error(format!("config: {warning}")))
            .ok();
    }
    tokio::spawn(ai::run_agent(user_rx, ai_tx, cfg.clone()));

    let app = ui::App::new(user_tx, ai_rx, cfg);
    tokio::task::block_in_place(|| {
        let mut terminal = ratatui::init();
        execute!(std::io::stdout(), EnableMouseCapture).ok();
        let result = app.run(&mut terminal);
        execute!(std::io::stdout(), DisableMouseCapture).ok();
        ratatui::restore();
        result
    })?;

    Ok(())
}
