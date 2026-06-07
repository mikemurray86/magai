pub const COMMANDS: &[(&str, &str)] = &[
    ("/clear", "clear conversation history"),
    ("/exit", "quit the application"),
    ("/help", "show available commands"),
    ("/model", "show or set the AI model"),
    ("/plugins", "list loaded plugins"),
    ("/provider", "list all models for a provider"),
    ("/tools", "enable or disable tools (on/off)"),
    ("/undo", "revert last agent changes via git stash pop"),
    ("/quit", "quit the application"),
];

pub fn matching_commands(prefix: &str, skills: &[crate::skills::Skill]) -> Vec<(String, String)> {
    let mut results: Vec<(String, String)> = COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, desc)| (name.to_string(), desc.to_string()))
        .collect();
    for skill in skills {
        let slash_name = format!("/{}", skill.name);
        if slash_name.starts_with(prefix) {
            results.push((slash_name, skill.description.clone()));
        }
    }
    results
}

pub enum SlashCommandAction {
    Exit,
    SetModel(String),
    ShowModel,
    SetTools(bool),
    Clear,
    Undo,
    ListModels(String),
    ShowMessage(String),
    ShowPlugins,
    RunSkill(String),
    Unknown(String),
}

pub fn dispatch(input: &str, skills: &[crate::skills::Skill]) -> SlashCommandAction {
    let mut iter = input.splitn(2, char::is_whitespace);
    let cmd = iter.next().unwrap_or(input);
    let arg = iter.next().unwrap_or("").trim();

    match cmd {
        "/exit" | "/quit" => SlashCommandAction::Exit,
        "/clear" => SlashCommandAction::Clear,
        "/undo" => SlashCommandAction::Undo,
        "/plugins" => SlashCommandAction::ShowPlugins,
        "/model" => {
            if arg.is_empty() {
                SlashCommandAction::ShowModel
            } else {
                SlashCommandAction::SetModel(arg.to_string())
            }
        }
        "/provider" => {
            if arg.is_empty() {
                SlashCommandAction::ShowMessage("usage: /provider <alias>".to_string())
            } else {
                SlashCommandAction::ListModels(arg.to_string())
            }
        }
        "/tools" => match arg {
            "on" => SlashCommandAction::SetTools(true),
            "off" => SlashCommandAction::SetTools(false),
            _ => SlashCommandAction::ShowMessage("usage: /tools on|off".to_string()),
        },
        "/help" => SlashCommandAction::ShowMessage(
            "/clear             —  clear conversation\n\
             /exit, /quit       —  quit the application\n\
             /help              —  show this message\n\
             /model [name]      —  show or set the AI model\n\
             /plugins           —  list loaded plugins\n\
             /tools on|off      —  enable or disable tools\n\
             /undo              —  revert last agent changes (git stash pop)"
                .to_string(),
        ),
        _ => {
            let skill_name = cmd.trim_start_matches('/');
            if let Some(skill) = skills.iter().find(|s| s.name == skill_name) {
                SlashCommandAction::RunSkill(crate::skills::render(&skill.content, arg))
            } else {
                SlashCommandAction::Unknown(format!("unknown command: {cmd}"))
            }
        }
    }
}
