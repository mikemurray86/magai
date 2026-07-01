//! Keyboard/mouse event handling: scrolling, slash-command dispatch,
//! autocomplete navigation (model/provider/provider-model), the
//! tool-approval-card key intercept, and message submission.

use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use std::time::Duration;

use super::{make_textarea, App, ChatMessage, Role};

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

impl App {
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
                self.user_tx.send(crate::ai::AgentCommand::Undo).ok();
            }
            SlashCommandAction::Squash(message) => {
                self.user_tx
                    .send(crate::ai::AgentCommand::Squash(message))
                    .ok();
            }
            SlashCommandAction::ShowModel => {
                self.push_system(format!("current model: {}", self.current_model));
            }
            SlashCommandAction::RunSkill(content) => {
                if self
                    .input_history
                    .last()
                    .map(|s| s != input)
                    .unwrap_or(true)
                {
                    self.input_history.push(input.to_string());
                }
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
            SlashCommandAction::MemorySearch(query) => {
                let msg = memory_search(&self.config, &query);
                self.push_system(msg);
            }
            SlashCommandAction::MemoryClear => {
                let msg = memory_clear(&self.config);
                self.push_system(msg);
            }
            SlashCommandAction::ShowMessage(msg) | SlashCommandAction::Unknown(msg) => {
                self.push_system(msg);
            }
        }
    }

    fn submit_input(&mut self) {
        let input = self.textarea.lines().join("\n");
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        self.textarea = make_textarea("", false);
        self.history_cursor = None;

        if input.starts_with('/') {
            self.handle_slash_command(&input);
        } else {
            if self
                .input_history
                .last()
                .map(|s| s != &input)
                .unwrap_or(true)
            {
                self.input_history.push(input.clone());
            }
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

                    // Dirty workspace prompt intercepts s/c exclusively
                    if self.pending_dirty_workspace {
                        match key.code {
                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                self.user_tx
                                    .send(crate::ai::AgentCommand::DirtyWorkspaceResponse(true))
                                    .ok();
                                self.pending_dirty_workspace = false;
                            }
                            KeyCode::Char('c') | KeyCode::Char('C') => {
                                self.user_tx
                                    .send(crate::ai::AgentCommand::DirtyWorkspaceResponse(false))
                                    .ok();
                                self.pending_dirty_workspace = false;
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
                                self.textarea = make_textarea("", false);
                            } else {
                                self.textarea = make_textarea("", false);
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
                                    self.textarea = make_textarea(&model_id, false);
                                }
                            } else {
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    let idx =
                                        self.model_ac_idx.unwrap_or(0).min(candidates.len() - 1);
                                    let alias = candidates[idx].alias.clone();
                                    self.textarea =
                                        make_textarea(&format!("/model {alias}"), false);
                                    self.model_ac_idx = Some(idx);
                                } else {
                                    let provider_names = self.provider_ac_candidates();
                                    if !provider_names.is_empty() {
                                        let idx = self
                                            .provider_ac_idx
                                            .unwrap_or(0)
                                            .min(provider_names.len() - 1);
                                        let name = provider_names[idx].clone();
                                        self.textarea =
                                            make_textarea(&format!("/provider {name}"), false);
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
                                self.textarea = make_textarea("", false);
                            } else {
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    if let Some(idx) = self.model_ac_idx {
                                        let alias =
                                            candidates[idx.min(candidates.len() - 1)].alias.clone();
                                        self.textarea =
                                            make_textarea(&format!("/model {alias}"), false);
                                    }
                                } else {
                                    let provider_names = self.provider_ac_candidates();
                                    if !provider_names.is_empty() {
                                        if let Some(idx) = self.provider_ac_idx {
                                            let name = provider_names
                                                [idx.min(provider_names.len() - 1)]
                                            .clone();
                                            self.textarea =
                                                make_textarea(&format!("/provider {name}"), false);
                                        }
                                    }
                                }
                                self.model_ac_idx = None;
                                self.provider_ac_idx = None;
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
                            } else if self.textarea.lines().len() == 1 {
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
                                    } else if !self.input_history.is_empty() {
                                        let idx = match self.history_cursor {
                                            None => self.input_history.len() - 1,
                                            Some(i) => i.saturating_sub(1),
                                        };
                                        self.history_cursor = Some(idx);
                                        let text = self.input_history[idx].clone();
                                        self.textarea = make_textarea(&text, false);
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
                            } else if self.textarea.lines().len() == 1 {
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
                                    } else if let Some(idx) = self.history_cursor {
                                        if idx + 1 < self.input_history.len() {
                                            let next = idx + 1;
                                            self.history_cursor = Some(next);
                                            let text = self.input_history[next].clone();
                                            self.textarea = make_textarea(&text, false);
                                        } else {
                                            self.history_cursor = None;
                                            self.textarea = make_textarea("", false);
                                        }
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
