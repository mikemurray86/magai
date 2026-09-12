pub const COMMANDS: &[(&str, &str)] = &[
    ("/clear", "clear conversation history"),
    ("/exit", "quit the application"),
    ("/help", "show available commands"),
    ("/mcp", "list configured MCP servers and their status"),
    ("/memory", "search or manage persistent memory"),
    ("/model", "show or set the AI model"),
    ("/plugins", "list loaded plugins"),
    ("/provider", "list all models for a provider"),
    (
        "/rate",
        "rate the last turn (good/bad/neutral) for fine-tuning data",
    ),
    (
        "/squash",
        "collapse checkpoint commits into one (optional message)",
    ),
    ("/tools", "enable or disable tools (on/off)"),
    ("/undo", "revert last agent turn (git reset --hard HEAD~1)"),
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

/// Longest common prefix shared by every command matching `prefix`.
///
/// `/q` with only `/quit` matching yields `"/quit"`; `/me` with `/memory` and
/// `/memory-clear` matching yields `"/memory"`. `None` when nothing matches.
pub fn common_completion(prefix: &str, skills: &[crate::skills::Skill]) -> Option<String> {
    let matches = matching_commands(prefix, skills);
    let (first, _) = matches.first()?;
    let mut common = first.clone();
    for (name, _) in matches.iter().skip(1) {
        let len: usize = common
            .chars()
            .zip(name.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum();
        common.truncate(len);
    }
    Some(common)
}

pub enum SlashCommandAction {
    Exit,
    SetModel(String),
    ShowModel,
    SetTools(bool),
    Clear,
    Undo,
    Squash(String),
    ListModels(String),
    ShowMessage(String),
    ShowPlugins,
    ShowMcp,
    RunSkill(String),
    MemorySearch(String),
    MemoryClear,
    Rate(String, String),
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
        "/squash" => SlashCommandAction::Squash(arg.to_string()),
        "/plugins" => SlashCommandAction::ShowPlugins,
        "/mcp" => SlashCommandAction::ShowMcp,
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
        "/memory" => match arg {
            "clear" => SlashCommandAction::MemoryClear,
            _ => SlashCommandAction::MemorySearch(arg.to_string()),
        },
        "/rate" => {
            let mut it = arg.splitn(2, char::is_whitespace);
            let verdict = it.next().unwrap_or("");
            let note = it.next().unwrap_or("").trim().to_string();
            match verdict {
                "good" | "bad" | "neutral" => SlashCommandAction::Rate(verdict.to_string(), note),
                _ => SlashCommandAction::ShowMessage(
                    "usage: /rate good|bad|neutral [note]".to_string(),
                ),
            }
        }
        "/help" => SlashCommandAction::ShowMessage(
            "/clear             —  clear conversation\n\
             /exit, /quit       —  quit the application\n\
             /help              —  show this message\n\
             /mcp               —  list configured MCP servers and their status\n\
             /memory [query]    —  search memory graph; /memory clear to wipe\n\
             /model [name]      —  show or set the AI model\n\
             /plugins           —  list loaded plugins\n\
             /provider <alias>  —  list all models offered by a provider\n\
             /rate good|bad|neutral [note] —  rate the last turn for fine-tuning data\n\
             /squash [message]  —  collapse checkpoint commits; commits if message given\n\
             /tools on|off      —  enable or disable tools\n\
             /undo              —  revert last agent turn (git reset --hard HEAD~1)"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;

    fn skill(name: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            content: format!("running {{{{args}}}} for {name}"),
        }
    }

    #[test]
    fn matching_commands_filters_by_prefix() {
        let names: Vec<String> = matching_commands("/mod", &[])
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["/model".to_string()]);
    }

    #[test]
    fn matching_commands_includes_skills() {
        let skills = vec![skill("review"), skill("rename")];
        let names: Vec<String> = matching_commands("/re", &skills)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert!(names.contains(&"/review".to_string()));
        assert!(names.contains(&"/rename".to_string()));
    }

    #[test]
    fn matching_commands_empty_prefix_matches_everything() {
        let results = matching_commands("/", &[skill("foo")]);
        assert_eq!(results.len(), COMMANDS.len() + 1);
    }

    #[test]
    fn common_completion_unique_prefix_completes_fully() {
        assert_eq!(common_completion("/q", &[]).as_deref(), Some("/quit"));
    }

    #[test]
    fn common_completion_stops_at_shared_prefix() {
        let skills = vec![skill("review"), skill("rename")];
        assert_eq!(common_completion("/r", &skills).as_deref(), Some("/r"));
        assert_eq!(common_completion("/re", &skills).as_deref(), Some("/re"));
    }

    #[test]
    fn common_completion_none_when_no_match() {
        assert!(common_completion("/zzz", &[]).is_none());
    }

    #[test]
    fn dispatch_exit_and_quit() {
        assert!(matches!(dispatch("/exit", &[]), SlashCommandAction::Exit));
        assert!(matches!(dispatch("/quit", &[]), SlashCommandAction::Exit));
    }

    #[test]
    fn dispatch_simple_actions() {
        assert!(matches!(dispatch("/clear", &[]), SlashCommandAction::Clear));
        assert!(matches!(dispatch("/undo", &[]), SlashCommandAction::Undo));
        assert!(matches!(
            dispatch("/plugins", &[]),
            SlashCommandAction::ShowPlugins
        ));
        assert!(matches!(dispatch("/mcp", &[]), SlashCommandAction::ShowMcp));
    }

    #[test]
    fn help_lists_every_command() {
        let SlashCommandAction::ShowMessage(help) = dispatch("/help", &[]) else {
            panic!("/help should show a message");
        };
        for (name, _) in COMMANDS {
            assert!(help.contains(name), "/help omits {name}");
        }
    }

    #[test]
    fn dispatch_model_with_and_without_arg() {
        assert!(matches!(
            dispatch("/model", &[]),
            SlashCommandAction::ShowModel
        ));
        match dispatch("/model  gpt-4o ", &[]) {
            SlashCommandAction::SetModel(m) => assert_eq!(m, "gpt-4o"),
            _ => panic!("expected SetModel"),
        }
    }

    #[test]
    fn dispatch_provider_requires_arg() {
        assert!(matches!(
            dispatch("/provider", &[]),
            SlashCommandAction::ShowMessage(_)
        ));
        match dispatch("/provider openai", &[]) {
            SlashCommandAction::ListModels(p) => assert_eq!(p, "openai"),
            _ => panic!("expected ListModels"),
        }
    }

    #[test]
    fn dispatch_rate_with_and_without_note() {
        match dispatch("/rate good", &[]) {
            SlashCommandAction::Rate(verdict, note) => {
                assert_eq!(verdict, "good");
                assert_eq!(note, "");
            }
            _ => panic!("expected Rate"),
        }
        match dispatch("/rate bad would loop forever", &[]) {
            SlashCommandAction::Rate(verdict, note) => {
                assert_eq!(verdict, "bad");
                assert_eq!(note, "would loop forever");
            }
            _ => panic!("expected Rate"),
        }
        assert!(matches!(
            dispatch("/rate maybe", &[]),
            SlashCommandAction::ShowMessage(_)
        ));
        assert!(matches!(
            dispatch("/rate", &[]),
            SlashCommandAction::ShowMessage(_)
        ));
    }

    #[test]
    fn dispatch_tools_on_off_and_invalid() {
        assert!(matches!(
            dispatch("/tools on", &[]),
            SlashCommandAction::SetTools(true)
        ));
        assert!(matches!(
            dispatch("/tools off", &[]),
            SlashCommandAction::SetTools(false)
        ));
        assert!(matches!(
            dispatch("/tools maybe", &[]),
            SlashCommandAction::ShowMessage(_)
        ));
    }

    #[test]
    fn dispatch_help_shows_message() {
        assert!(matches!(
            dispatch("/help", &[]),
            SlashCommandAction::ShowMessage(_)
        ));
    }

    #[test]
    fn dispatch_runs_matching_skill_with_args() {
        let skills = vec![skill("greet")];
        match dispatch("/greet world", &skills) {
            SlashCommandAction::RunSkill(rendered) => {
                assert_eq!(rendered, "running world for greet");
            }
            _ => panic!("expected RunSkill"),
        }
    }

    #[test]
    fn dispatch_squash_with_and_without_message() {
        match dispatch("/squash feat: add login", &[]) {
            SlashCommandAction::Squash(msg) => assert_eq!(msg, "feat: add login"),
            _ => panic!("expected Squash"),
        }
        match dispatch("/squash", &[]) {
            SlashCommandAction::Squash(msg) => assert_eq!(msg, ""),
            _ => panic!("expected Squash"),
        }
    }

    #[test]
    fn dispatch_unknown_command() {
        match dispatch("/bogus", &[]) {
            SlashCommandAction::Unknown(msg) => assert!(msg.contains("/bogus")),
            _ => panic!("expected Unknown"),
        }
    }
}
