//! Keyboard/mouse event handling: scrolling, slash-command dispatch,
//! autocomplete navigation (model/provider/provider-model), the
//! tool-approval-card key intercept, and message submission.

use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use std::time::Duration;

use super::{make_textarea, App, ChatMessage, Role};
use ratatui_textarea::CursorMove;

fn memory_db_for(config: &crate::config::Config) -> Option<crate::memory::MemoryDb> {
    if !config.memory.enabled {
        return None;
    }
    let path = config
        .memory
        .db_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(crate::memory::default_db_path);
    crate::memory::MemoryDb::open(&path).ok()
}

fn memory_search(config: &crate::config::Config, query: &str) -> String {
    let Some(db) = memory_db_for(config) else {
        return "memory is disabled".to_string();
    };
    if query.is_empty() {
        let lines = crate::memory::retrieval::recent_nodes(&db, 15);
        if lines.is_empty() {
            "memory is empty".to_string()
        } else {
            format!("Recent memory:\n{}", lines.join("\n"))
        }
    } else {
        let lines = crate::memory::retrieval::query_with_neighbors(&db, query, 10);
        if lines.is_empty() {
            format!("no memory entries matching \"{query}\"")
        } else {
            lines.join("\n")
        }
    }
}

fn memory_clear(config: &crate::config::Config) -> String {
    let Some(db) = memory_db_for(config) else {
        return "memory is disabled".to_string();
    };
    match crate::memory::retrieval::clear_all(&db) {
        Ok(()) => "memory cleared".to_string(),
        Err(e) => format!("memory clear failed: {e}"),
    }
}

fn rate_turn(config: &crate::config::Config, turn_id: &str, verdict: &str, note: &str) -> String {
    if !config.quality.enabled {
        return "quality tracking is disabled ([quality] enabled = true to turn on)".to_string();
    }
    let Some(db) = memory_db_for(config) else {
        return "quality tracking is disabled".to_string();
    };
    let rationale = (!note.is_empty()).then_some(note);
    crate::memory::quality::record_rating(&db, turn_id, "user", Some(verdict), None, rationale);
    format!("rated last turn: {verdict}")
}

impl App {
    /// Checkpoint commands go to the agent task, which owns the store. While a
    /// turn is streaming, `drive_stream` drops every non-approval command, so
    /// refuse here instead of letting the command disappear silently.
    fn send_checkpoint(&mut self, cmd: crate::ai::AgentCommand) {
        if self.is_waiting {
            self.push_system("busy — finish or cancel the turn first".to_string());
            return;
        }
        self.user_tx.send(cmd).ok();
    }

    fn handle_slash_command(&mut self, input: &str) {
        use crate::slash_commands::{dispatch, SlashCommandAction};
        match dispatch(input, &self.skills) {
            SlashCommandAction::Exit => self.exit = true,
            SlashCommandAction::SetModel(model) => {
                self.current_model = model.clone();
                self.user_tx
                    .send(crate::ai::AgentCommand::SetModel(model.clone()))
                    .ok();
                self.push_system(format!("model set to {model}"));
            }
            SlashCommandAction::SetTools(enabled) => {
                self.user_tx
                    .send(crate::ai::AgentCommand::SetTools(enabled))
                    .ok();
                self.push_system(format!(
                    "tools {}",
                    if enabled { "enabled" } else { "disabled" }
                ));
            }
            SlashCommandAction::Clear => {
                self.user_tx.send(crate::ai::AgentCommand::Clear).ok();
            }
            SlashCommandAction::Undo => {
                self.send_checkpoint(crate::ai::AgentCommand::CheckpointPreview(
                    crate::checkpoint::CheckpointAction::Undo,
                ));
            }
            SlashCommandAction::Redo => {
                // No confirmation card: a redo is already an explicit inverse
                // of something the user just chose to undo.
                self.send_checkpoint(crate::ai::AgentCommand::CheckpointApply(
                    crate::checkpoint::CheckpointAction::Redo,
                ));
            }
            SlashCommandAction::Restore(id) => {
                self.send_checkpoint(crate::ai::AgentCommand::CheckpointPreview(
                    crate::checkpoint::CheckpointAction::Restore(id),
                ));
            }
            SlashCommandAction::Checkpoints => {
                self.send_checkpoint(crate::ai::AgentCommand::CheckpointList);
            }
            SlashCommandAction::Diff(id) => {
                self.send_checkpoint(crate::ai::AgentCommand::CheckpointDiff(id));
            }
            SlashCommandAction::ShowModel => {
                self.push_system(format!("current model: {}", self.current_model));
            }
            SlashCommandAction::RunSkill(content) => {
                self.record_history(input);
                self.messages.push(ChatMessage {
                    role: Role::User,
                    content: content.clone(),
                    model_name: None,
                    call_id: None,
                    elapsed_ms: None,
                });
                self.user_tx
                    .send(crate::ai::AgentCommand::Message(content))
                    .ok();
                self.is_waiting = true;
                self.auto_scroll = true;
            }
            SlashCommandAction::ListModels(alias) => {
                self.push_system(format!("fetching models for {alias}…"));
                self.user_tx
                    .send(crate::ai::AgentCommand::ListModels(alias))
                    .ok();
            }
            SlashCommandAction::ShowPlugins => {
                self.push_system(crate::plugins::summary(&self.plugins));
            }
            SlashCommandAction::Config => {
                // Needs the terminal, which only `App::run` holds.
                self.run_setup = true;
            }
            SlashCommandAction::ShowMcp => {
                // status lives with the agent task, so it answers via AiEvent
                self.user_tx.send(crate::ai::AgentCommand::ListMcp).ok();
            }
            SlashCommandAction::MemorySearch(query) => {
                let msg = memory_search(&self.config, &query);
                self.push_system(msg);
            }
            SlashCommandAction::MemoryClear => {
                let msg = memory_clear(&self.config);
                self.push_system(msg);
            }
            SlashCommandAction::Rate(verdict, note) => {
                let Some(turn_id) = self.last_turn_id.clone() else {
                    self.push_system("nothing to rate yet".to_string());
                    return;
                };
                let msg = rate_turn(&self.config, &turn_id, &verdict, &note);
                self.push_system(msg);
            }
            SlashCommandAction::Theme(None) => {
                let list: Vec<String> = super::theme::available_themes(&self.config.themes)
                    .into_iter()
                    .map(|name| {
                        let mark = if name == self.theme.name { "*" } else { " " };
                        format!("{mark} {name}")
                    })
                    .collect();
                self.push_system(format!(
                    "themes (set `theme = \"<name>\"` in config.toml to keep one):\n{}",
                    list.join("\n")
                ));
            }
            SlashCommandAction::Theme(Some(name)) => {
                let known = super::theme::available_themes(&self.config.themes);
                if !known.contains(&name) {
                    self.push_system(format!(
                        "unknown theme {name:?} (available: {})",
                        known.join(", ")
                    ));
                } else {
                    let (theme, warnings) =
                        super::theme::Theme::resolve(&name, &self.config.themes);
                    self.theme = theme;
                    for w in warnings {
                        self.push_system(format!("config: {w}"));
                    }
                    self.push_system(format!("theme set to {name}"));
                }
            }
            SlashCommandAction::ShowMessage(msg) | SlashCommandAction::Unknown(msg) => {
                self.push_system(msg);
            }
        }
    }

    /// Tab-completion for the slash-command popup: fills the longest prefix
    /// shared by all matches (which completes a unique match outright), and
    /// cycles through the candidates once there's nothing more to share.
    fn complete_slash_command(&mut self) {
        let cmds = self.cmd_ac_candidates();
        if cmds.is_empty() {
            return;
        }
        if let Some(i) = self.cmd_ac_idx {
            let next = (i + 1) % cmds.len();
            self.cmd_ac_idx = Some(next);
            let name = cmds[next].0.clone();
            self.textarea = make_textarea(&name);
            return;
        }
        let line = self.textarea.lines().first().cloned().unwrap_or_default();
        let common = crate::slash_commands::common_completion(&line, &self.skills)
            .unwrap_or_else(|| line.clone());
        if common.len() > line.len() {
            self.textarea = make_textarea(&common);
            if cmds.len() == 1 {
                self.cmd_ac_idx = Some(0);
            }
        } else {
            self.cmd_ac_idx = Some(0);
            let name = cmds[0].0.clone();
            self.textarea = make_textarea(&name);
        }
    }

    /// Adds a sent prompt to the persistent history. A save failure is shown
    /// but doesn't block the message; the entry still recalls this session.
    fn record_history(&mut self, input: &str) {
        if let Err(e) = self.input_history.push(input) {
            self.push_system(e);
        }
    }

    /// Replaces the input with the next-older history entry.
    fn history_prev(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let idx = match self.history_cursor {
            None => self.input_history.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.history_cursor = Some(idx);
        let text = self.input_history.get(idx).unwrap_or_default().to_string();
        self.textarea = make_textarea(&text);
    }

    /// Replaces the input with the next-newer history entry, or clears it
    /// when stepping past the newest.
    fn history_next(&mut self) {
        let Some(idx) = self.history_cursor else {
            return;
        };
        if idx + 1 < self.input_history.len() {
            self.history_cursor = Some(idx + 1);
            let text = self
                .input_history
                .get(idx + 1)
                .unwrap_or_default()
                .to_string();
            self.textarea = make_textarea(&text);
        } else {
            self.history_cursor = None;
            self.textarea = make_textarea("");
        }
    }

    fn submit_input(&mut self) {
        let input = self.textarea.lines().join("\n");
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        self.textarea = make_textarea("");
        self.history_cursor = None;

        if input.starts_with('/') {
            self.handle_slash_command(&input);
        } else {
            self.record_history(&input);
            self.messages.push(ChatMessage {
                role: Role::User,
                content: input.clone(),
                model_name: None,
                call_id: None,
                elapsed_ms: None,
            });
            self.user_tx
                .send(crate::ai::AgentCommand::Message(input))
                .ok();
            self.is_waiting = true;
            self.auto_scroll = true;
        }
    }

    pub(super) fn handle_events(&mut self) -> std::io::Result<()> {
        if poll(Duration::from_millis(50))? {
            match read()? {
                Event::Mouse(mouse) => {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            self.auto_scroll = false;
                            self.scroll = self.scroll.saturating_sub(3);
                        }
                        MouseEventKind::ScrollDown => {
                            let max = self.content_lines.saturating_sub(self.view_height);
                            self.scroll = (self.scroll + 3).min(max);
                            if self.scroll >= max {
                                self.auto_scroll = true;
                            }
                        }
                        _ => {}
                    }
                    return Ok(());
                }
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        return Ok(());
                    }

                    // Max-turns prompt intercepts digits/Backspace/Enter/Esc exclusively
                    if self.pending_max_turns.is_some() {
                        match key.code {
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                self.max_turns_input.push(c);
                            }
                            KeyCode::Backspace => {
                                self.max_turns_input.pop();
                            }
                            KeyCode::Enter => {
                                let n = if self.max_turns_input.is_empty() {
                                    10
                                } else {
                                    self.max_turns_input.parse().unwrap_or(10)
                                };
                                self.user_tx
                                    .send(crate::ai::AgentCommand::MaxTurnsResponse(Some(n)))
                                    .ok();
                                self.pending_max_turns = None;
                                self.max_turns_input.clear();
                            }
                            KeyCode::Esc => {
                                self.user_tx
                                    .send(crate::ai::AgentCommand::MaxTurnsResponse(None))
                                    .ok();
                                self.pending_max_turns = None;
                                self.max_turns_input.clear();
                            }
                            _ => {}
                        }
                        return Ok(());
                    }

                    // Checkpoint confirmation intercepts y/n exclusively
                    if let Some(prompt) = self.pending_checkpoint.clone() {
                        // A blocked action can only be dismissed; Esc must
                        // always work, since this block also swallows Ctrl+C.
                        if prompt.blocked.is_some() {
                            self.pending_checkpoint = None;
                            return Ok(());
                        }
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                self.user_tx
                                    .send(crate::ai::AgentCommand::CheckpointApply(prompt.action))
                                    .ok();
                                self.pending_checkpoint = None;
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                self.pending_checkpoint = None;
                                self.push_system("cancelled".to_string());
                            }
                            _ => {}
                        }
                        return Ok(());
                    }

                    // Approval card intercepts y/n exclusively
                    if let Some(ref approval) = self.pending_approval.clone() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                self.user_tx
                                    .send(crate::ai::AgentCommand::ApproveToolCall(
                                        approval.call_id.clone(),
                                    ))
                                    .ok();
                                self.pending_approval = None;
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                self.user_tx
                                    .send(crate::ai::AgentCommand::DenyToolCall(
                                        approval.call_id.clone(),
                                    ))
                                    .ok();
                                self.pending_approval = None;
                            }
                            _ => {}
                        }
                        return Ok(());
                    }

                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

                    match key.code {
                        // Always: Ctrl+C exits
                        KeyCode::Char('c') if ctrl => {
                            self.exit = true;
                        }

                        // Always: PageUp/PageDown/Ctrl+Up/Ctrl+Down scroll history
                        KeyCode::PageUp | KeyCode::Up if ctrl => {
                            self.auto_scroll = false;
                            self.scroll = self.scroll.saturating_sub(self.view_height);
                        }
                        KeyCode::PageDown | KeyCode::Down if ctrl => {
                            let max = self.content_lines.saturating_sub(self.view_height);
                            self.scroll = (self.scroll + self.view_height).min(max);
                            if self.scroll >= max {
                                self.auto_scroll = true;
                            }
                        }
                        KeyCode::PageUp => {
                            self.auto_scroll = false;
                            self.scroll = self.scroll.saturating_sub(self.view_height);
                        }
                        KeyCode::PageDown => {
                            let max = self.content_lines.saturating_sub(self.view_height);
                            self.scroll = (self.scroll + self.view_height).min(max);
                            if self.scroll >= max {
                                self.auto_scroll = true;
                            }
                        }

                        // Esc: cancel when waiting / dismiss provider popup / clear textarea
                        KeyCode::Esc => {
                            if self.is_waiting {
                                self.user_tx.send(crate::ai::AgentCommand::Cancel).ok();
                                self.push_system("[cancelled]".into());
                                self.is_waiting = false;
                            } else if self.provider_models.is_some() {
                                self.provider_models = None;
                                self.textarea = make_textarea("");
                            } else {
                                self.textarea = make_textarea("");
                                self.history_cursor = None;
                            }
                        }

                        // Tab: fill provider model / fill model AC / fill provider name AC
                        KeyCode::Tab if !self.is_waiting => {
                            if self.provider_models.is_some() {
                                let filtered = self.provider_model_filtered();
                                if !filtered.is_empty() {
                                    let sel = self.provider_model_sel.min(filtered.len() - 1);
                                    let model_id = filtered[sel].to_string();
                                    self.textarea = make_textarea(&model_id);
                                }
                            } else if !self.cmd_ac_candidates().is_empty() {
                                self.complete_slash_command();
                            } else {
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    let idx =
                                        self.model_ac_idx.unwrap_or(0).min(candidates.len() - 1);
                                    let alias = candidates[idx].alias.clone();
                                    self.textarea = make_textarea(&format!("/model {alias}"));
                                    self.model_ac_idx = Some(idx);
                                } else {
                                    let provider_names = self.provider_ac_candidates();
                                    if !provider_names.is_empty() {
                                        let idx = self
                                            .provider_ac_idx
                                            .unwrap_or(0)
                                            .min(provider_names.len() - 1);
                                        let name = provider_names[idx].clone();
                                        self.textarea = make_textarea(&format!("/provider {name}"));
                                        self.provider_ac_idx = Some(idx);
                                    }
                                }
                            }
                        }

                        // Enter: use provider model / fill model AC + submit / fill provider AC + submit / submit
                        KeyCode::Enter if !self.is_waiting => {
                            if shift {
                                self.textarea.insert_newline();
                                self.history_cursor = None;
                            } else if self.provider_models.is_some() {
                                let filtered = self.provider_model_filtered();
                                if !filtered.is_empty() {
                                    let sel = self.provider_model_sel.min(filtered.len() - 1);
                                    let model_id = filtered[sel].to_string();
                                    let provider_alias =
                                        self.provider_models.as_ref().unwrap().0.clone();
                                    self.current_model = format!("{provider_alias}/{model_id}");
                                    self.push_system(format!(
                                        "model set to {}",
                                        self.current_model
                                    ));
                                    self.user_tx
                                        .send(crate::ai::AgentCommand::UseProviderModel {
                                            provider_alias,
                                            model_id,
                                        })
                                        .ok();
                                }
                                self.provider_models = None;
                                self.textarea = make_textarea("");
                            } else {
                                let cmds = self.cmd_ac_candidates();
                                if !cmds.is_empty() {
                                    let line =
                                        self.textarea.lines().first().cloned().unwrap_or_default();
                                    if let Some(idx) = self.cmd_ac_idx {
                                        let name = cmds[idx.min(cmds.len() - 1)].0.clone();
                                        self.textarea = make_textarea(&name);
                                    } else if cmds.len() == 1 && cmds[0].0 != line {
                                        // unambiguous: `/q` submits as `/quit`
                                        let name = cmds[0].0.clone();
                                        self.textarea = make_textarea(&name);
                                    }
                                }
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    if let Some(idx) = self.model_ac_idx {
                                        let alias =
                                            candidates[idx.min(candidates.len() - 1)].alias.clone();
                                        self.textarea = make_textarea(&format!("/model {alias}"));
                                    }
                                } else {
                                    let provider_names = self.provider_ac_candidates();
                                    if !provider_names.is_empty() {
                                        if let Some(idx) = self.provider_ac_idx {
                                            let name = provider_names
                                                [idx.min(provider_names.len() - 1)]
                                            .clone();
                                            self.textarea =
                                                make_textarea(&format!("/provider {name}"));
                                        }
                                    }
                                }
                                self.model_ac_idx = None;
                                self.provider_ac_idx = None;
                                self.cmd_ac_idx = None;
                                self.submit_input();
                            }
                        }

                        // Up/Down: provider model nav / model AC / provider AC / history / scroll
                        KeyCode::Up if !self.is_waiting => {
                            if self.provider_models.is_some() {
                                let n = self.provider_model_filtered().len();
                                if n > 0 {
                                    self.provider_model_sel = if self.provider_model_sel == 0 {
                                        n - 1
                                    } else {
                                        self.provider_model_sel - 1
                                    };
                                }
                            } else if self.textarea.cursor().0 > 0 {
                                // Multi-line input: walk up to the top line
                                // before Up means "older history".
                                self.textarea.move_cursor(CursorMove::Up);
                            } else if self.textarea.lines().len() > 1 {
                                self.history_prev();
                            } else {
                                let cmds = self.cmd_ac_candidates();
                                if !cmds.is_empty() {
                                    let n = cmds.len();
                                    self.cmd_ac_idx = Some(match self.cmd_ac_idx {
                                        None | Some(0) => n - 1,
                                        Some(i) => i - 1,
                                    });
                                    return Ok(());
                                }
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    let n = candidates.len();
                                    self.model_ac_idx = Some(match self.model_ac_idx {
                                        None | Some(0) => n - 1,
                                        Some(i) => i - 1,
                                    });
                                } else {
                                    let provider_names = self.provider_ac_candidates();
                                    if !provider_names.is_empty() {
                                        let n = provider_names.len();
                                        self.provider_ac_idx = Some(match self.provider_ac_idx {
                                            None | Some(0) => n - 1,
                                            Some(i) => i - 1,
                                        });
                                    } else {
                                        self.history_prev();
                                    }
                                }
                            }
                        }
                        KeyCode::Down if !self.is_waiting => {
                            if self.provider_models.is_some() {
                                let n = self.provider_model_filtered().len();
                                if n > 0 {
                                    self.provider_model_sel = (self.provider_model_sel + 1) % n;
                                }
                            } else if self.textarea.cursor().0 + 1 < self.textarea.lines().len() {
                                self.textarea.move_cursor(CursorMove::Down);
                            } else if self.textarea.lines().len() > 1 {
                                self.history_next();
                            } else {
                                let cmds = self.cmd_ac_candidates();
                                if !cmds.is_empty() {
                                    let n = cmds.len();
                                    self.cmd_ac_idx = Some(match self.cmd_ac_idx {
                                        None => 0,
                                        Some(i) => (i + 1) % n,
                                    });
                                    return Ok(());
                                }
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    let n = candidates.len();
                                    self.model_ac_idx = Some(match self.model_ac_idx {
                                        None => 0,
                                        Some(i) => (i + 1) % n,
                                    });
                                } else {
                                    let provider_names = self.provider_ac_candidates();
                                    if !provider_names.is_empty() {
                                        let n = provider_names.len();
                                        self.provider_ac_idx = Some(match self.provider_ac_idx {
                                            None => 0,
                                            Some(i) => (i + 1) % n,
                                        });
                                    } else {
                                        self.history_next();
                                    }
                                }
                            }
                        }
                        KeyCode::Up if self.is_waiting => {
                            self.auto_scroll = false;
                            self.scroll = self.scroll.saturating_sub(1);
                        }
                        KeyCode::Down if self.is_waiting => {
                            self.scroll = self.scroll.saturating_add(1);
                            if self.scroll >= self.content_lines.saturating_sub(self.view_height) {
                                self.auto_scroll = true;
                            }
                        }

                        // Everything else: forward to textarea; reset all AC selections
                        _ if !self.is_waiting => {
                            if self.provider_models.is_some() {
                                // typing filters the provider model list
                                self.textarea.input(key);
                                self.provider_model_sel = 0;
                            } else {
                                self.textarea.input(key);
                                self.history_cursor = None;
                                self.model_ac_idx = None;
                                self.provider_ac_idx = None;
                                self.cmd_ac_idx = None;
                            }
                        }

                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
