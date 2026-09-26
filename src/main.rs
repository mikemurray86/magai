mod ai;
mod approval;
mod checkpoint;
mod cli;
mod config;
mod history;
mod hooks;
mod mcp;
mod memory;
mod plugins;
mod setup;
mod skills;
mod slash_commands;
mod tools;
mod ui;

use clap::Parser;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use tokio::sync::mpsc;
use ui::AiEvent;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A subcommand runs in the terminal and exits; bare `magai` opens the TUI.
    if let Some(command) = cli::Cli::parse().command {
        if let Err(e) = cli::run(command).await {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // First launch: offer the setup wizard while stdin is still a plain terminal.
    tokio::task::block_in_place(setup::offer_first_run);

    let (user_tx, user_rx) = mpsc::unbounded_channel::<ai::AgentCommand>();
    let (ai_tx, ai_rx) = mpsc::unbounded_channel::<AiEvent>();

    let (cfg, config_warning) = config::Config::load();
    if let Some(warning) = config_warning {
        ai_tx
            .send(AiEvent::Error(format!("config: {warning}")))
            .ok();
    }
    // Resolve and validate the startup model *before* the alternate screen
    // takes over: a missing API key or an absent Ollama used to open a TUI
    // that looked healthy and failed on every turn, with stderr hidden.
    let startup = match ai::preflight_startup(&cfg).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let model_label = startup.display.clone();
    tokio::spawn(ai::run_agent(user_rx, ai_tx, cfg.clone(), startup));

    let app = ui::App::new(user_tx, ai_rx, cfg, model_label);
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
