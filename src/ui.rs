use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    DefaultTerminal, Frame,
};
use ratatui_textarea::TextArea;
use std::time::Duration;
use tokio::sync::mpsc;

pub enum AiEvent {
    ResponseStart(String),
    Token(String),
    Done,
    Error(String),
    ModelList {
        provider: String,
        models: Vec<String>,
    },
    ToolCallStart {
        call_id: String,
        name: String,
        args_json: String,
    },
    ToolCallResult {
        call_id: String,
        result: String,
        elapsed_ms: u64,
    },
    ToolCallApprovalRequired {
        call_id: String,
        name: String,
        args_json: String,
        is_dangerous: bool,
    },
    ContextTruncated {
        turns_dropped: usize,
    },
    HistoryCleared,
}

#[derive(Debug, Clone, PartialEq)]
enum Role {
    User,
    Assistant,
    System,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: Role,
    content: String,
    model_name: Option<String>,
    call_id: Option<String>,
    elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct PendingApproval {
    call_id: String,
    name: String,
    args_json: String,
    is_dangerous: bool,
}

pub struct App {
    messages: Vec<ChatMessage>,
    textarea: TextArea<'static>,
    exit: bool,
    is_waiting: bool,
    user_tx: mpsc::UnboundedSender<crate::ai::AgentCommand>,
    ai_rx: mpsc::UnboundedReceiver<AiEvent>,
    scroll: u16,
    content_lines: u16,
    view_height: u16,
    view_width: u16,
    auto_scroll: bool,
    current_model: String,
    pending_approval: Option<PendingApproval>,
    pending_model: Option<String>,
    input_history: Vec<String>,
    history_cursor: Option<usize>,
    skills: Vec<crate::skills::Skill>,
    plugins: Vec<crate::plugins::Plugin>,
    spinner_frame: u8,
    config: crate::config::Config,
    model_ac_idx: Option<usize>,
    provider_ac_idx: Option<usize>,
    provider_models: Option<(String, Vec<String>)>,
    provider_model_sel: usize,
}

impl App {
    pub fn new(
        user_tx: mpsc::UnboundedSender<crate::ai::AgentCommand>,
        ai_rx: mpsc::UnboundedReceiver<AiEvent>,
        config: crate::config::Config,
    ) -> Self {
        let plugins = crate::plugins::discover();
        let plugin_skills = crate::plugins::extract_skills(&plugins);
        let mut skills = crate::skills::discover();
        skills.extend(plugin_skills);

        Self {
            messages: Vec::new(),
            textarea: make_textarea(false),
            exit: false,
            is_waiting: false,
            user_tx,
            ai_rx,
            scroll: 0,
            content_lines: 0,
            view_height: 24,
            view_width: 80,
            auto_scroll: true,
            current_model: config
                .default_model
                .clone()
                .unwrap_or_else(|| crate::ai::DEFAULT_MODEL.to_string()),
            pending_approval: None,
            pending_model: None,
            input_history: Vec::new(),
            history_cursor: None,
            skills,
            plugins,
            spinner_frame: 0,
            config,
            model_ac_idx: None,
            provider_ac_idx: None,
            provider_models: None,
            provider_model_sel: 0,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        while !self.exit {
            self.poll_ai_events();
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn poll_ai_events(&mut self) {
        while let Ok(event) = self.ai_rx.try_recv() {
            match event {
                AiEvent::ResponseStart(model) => {
                    self.current_model = model.clone();
                    self.pending_model = Some(model);
                }
                AiEvent::Token(token) => {
                    if let Some(last) = self.messages.last_mut() {
                        if last.role == Role::Assistant {
                            last.content.push_str(&token);
                            continue;
                        }
                    }
                    // First token: create the message, claiming the pending model name
                    self.messages.push(ChatMessage {
                        role: Role::Assistant,
                        content: token,
                        model_name: self.pending_model.take(),
                        call_id: None,
                        elapsed_ms: None,
                    });
                }
                AiEvent::Done => {
                    self.is_waiting = false;
                    self.pending_approval = None;
                    self.pending_model = None;
                }
                AiEvent::Error(e) => {
                    self.messages.push(ChatMessage {
                        role: Role::System,
                        content: format!("error: {e}"),
                        model_name: None,
                        call_id: None,
                        elapsed_ms: None,
                    });
                    self.is_waiting = false;
                    self.pending_approval = None;
                }
                AiEvent::ToolCallStart {
                    call_id,
                    name,
                    args_json,
                } => {
                    self.messages.push(ChatMessage {
                        role: Role::ToolCall,
                        content: format_tool_call(&name, &args_json),
                        model_name: None,
                        call_id: Some(call_id),
                        elapsed_ms: None,
                    });
                    self.auto_scroll = true;
                }
                AiEvent::ToolCallResult {
                    call_id,
                    result,
                    elapsed_ms,
                } => {
                    // Update matching ToolCall message with result inline, or add ToolResult
                    let found = self.messages.iter_mut().rev().find(|m| {
                        m.role == Role::ToolCall && m.call_id.as_deref() == Some(&call_id)
                    });
                    if let Some(msg) = found {
                        msg.elapsed_ms = Some(elapsed_ms);
                    }
                    // Show result as a separate message
                    let summary = summarize_result(&result);
                    self.messages.push(ChatMessage {
                        role: Role::ToolResult,
                        content: summary,
                        model_name: None,
                        call_id: Some(call_id),
                        elapsed_ms: Some(elapsed_ms),
                    });
                    self.auto_scroll = true;
                }
                AiEvent::ToolCallApprovalRequired {
                    call_id,
                    name,
                    args_json,
                    is_dangerous,
                } => {
                    self.pending_approval = Some(PendingApproval {
                        call_id,
                        name,
                        args_json,
                        is_dangerous,
                    });
                    self.auto_scroll = true;
                }
                AiEvent::ContextTruncated { turns_dropped } => {
                    self.messages.push(ChatMessage {
                        role: Role::System,
                        content: format!("[context trimmed: dropped {turns_dropped} old turns]"),
                        model_name: None,
                        call_id: None,
                        elapsed_ms: None,
                    });
                }
                AiEvent::HistoryCleared => {
                    self.messages.clear();
                    self.messages.push(ChatMessage {
                        role: Role::System,
                        content: "conversation cleared".into(),
                        model_name: None,
                        call_id: None,
                        elapsed_ms: None,
                    });
                }
                AiEvent::ModelList { provider, models } => {
                    let count = models.len();
                    self.push_system(format!("{count} models from {provider} — ↑↓ navigate  Tab fill  Enter use  Esc dismiss"));
                    self.provider_models = Some((provider, models));
                    self.provider_model_sel = 0;
                    self.textarea = make_textarea(false);
                    self.model_ac_idx = None;
                }
            }
        }
    }

    fn model_ac_candidates(&self) -> Vec<&crate::config::NamedModel> {
        let first_line = self.textarea.lines().first().cloned().unwrap_or_default();
        let Some(partial) = first_line.strip_prefix("/model ") else {
            return vec![];
        };
        self.config
            .named_models
            .iter()
            .filter(|m| m.alias.starts_with(partial))
            .collect()
    }

    fn provider_ac_candidates(&self) -> Vec<String> {
        let first_line = self.textarea.lines().first().cloned().unwrap_or_default();
        let Some(partial) = first_line.strip_prefix("/provider ") else {
            return vec![];
        };
        let mut names: Vec<String> = self.config.providers.keys().cloned().collect();
        if !names.iter().any(|n| n == "ollama") {
            names.push("ollama".to_string());
        }
        names.sort();
        names.retain(|n| n.starts_with(partial));
        names
    }

    fn provider_model_filtered(&self) -> Vec<&str> {
        let Some((_, ref models)) = self.provider_models else {
            return vec![];
        };
        let filter = self
            .textarea
            .lines()
            .first()
            .map(|s| s.as_str())
            .unwrap_or("");
        models
            .iter()
            .filter(|m| filter.is_empty() || m.contains(filter))
            .map(|m| m.as_str())
            .collect()
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        let area = frame.area();
        let input_h = (self.textarea.lines().len() as u16).max(1).min(5);
        // Update textarea style to reflect waiting state
        self.textarea.set_style(if self.is_waiting {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::White)
        });
        let [title_area, sep_top, history_area, sep_bot, input_area] =
            area.layout(&Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Length(input_h),
            ]));

        self.view_height = history_area.height;
        self.view_width = history_area.width;

        // ── title ─────────────────────────────────────────
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "  magai",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  [{}]", self.current_model),
                    Style::default().fg(Color::DarkGray),
                ),
            ])),
            title_area,
        );

        // ── separators ────────────────────────────────────
        let rule = Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(Color::DarkGray),
        );
        frame.render_widget(Paragraph::new(Line::from(rule.clone())), sep_top);
        frame.render_widget(Paragraph::new(Line::from(rule)), sep_bot);

        // ── history ───────────────────────────────────────
        let lines = self.build_lines();
        self.content_lines = lines.len() as u16;

        if self.auto_scroll {
            self.scroll = self.content_lines.saturating_sub(self.view_height);
        }
        self.scroll = self
            .scroll
            .min(self.content_lines.saturating_sub(self.view_height));

        frame.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), history_area);

        // ── input ─────────────────────────────────────────
        frame.render_widget(&self.textarea, input_area);

        // ── command / model popup ─────────────────────────
        let first_line = self.textarea.lines().first().cloned().unwrap_or_default();
        if self.textarea.lines().len() == 1 && self.pending_approval.is_none() {
            let model_candidates = self.model_ac_candidates();
            if !model_candidates.is_empty() {
                // model autocomplete popup
                let popup_h = model_candidates.len() as u16 + 2;
                let popup_w = 50_u16.min(history_area.width.saturating_sub(4));
                let popup_rect = Rect::new(
                    history_area.x + 2,
                    input_area.y.saturating_sub(popup_h),
                    popup_w,
                    popup_h,
                );
                let selected = self.model_ac_idx;
                let popup_lines: Vec<Line> = model_candidates
                    .iter()
                    .enumerate()
                    .map(|(i, mc)| {
                        let is_sel = selected == Some(i);
                        let alias_style = if is_sel {
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        };
                        let desc_style = if is_sel {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };
                        let desc = format!("{}/{}", mc.provider, mc.model);
                        Line::from(vec![
                            Span::styled(format!(" {:<14}", mc.alias), alias_style),
                            Span::styled(desc, desc_style),
                        ])
                    })
                    .collect();
                frame.render_widget(Clear, popup_rect);
                frame.render_widget(
                    Paragraph::new(popup_lines).block(
                        Block::new()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::DarkGray)),
                    ),
                    popup_rect,
                );
            } else {
                let provider_names = self.provider_ac_candidates();
                if !provider_names.is_empty() {
                    // /provider <partial> autocomplete
                    let popup_h = provider_names.len() as u16 + 2;
                    let popup_w = 30_u16.min(history_area.width.saturating_sub(4));
                    let popup_rect = Rect::new(
                        history_area.x + 2,
                        input_area.y.saturating_sub(popup_h),
                        popup_w,
                        popup_h,
                    );
                    let selected = self.provider_ac_idx;
                    let popup_lines: Vec<Line> = provider_names
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let is_sel = selected == Some(i);
                            let style = if is_sel {
                                Style::default()
                                    .fg(Color::Black)
                                    .bg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            };
                            Line::from(Span::styled(format!(" {name}"), style))
                        })
                        .collect();
                    frame.render_widget(Clear, popup_rect);
                    frame.render_widget(
                        Paragraph::new(popup_lines).block(
                            Block::new()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::DarkGray)),
                        ),
                        popup_rect,
                    );
                } else if first_line.starts_with('/') {
                    let matches =
                        crate::slash_commands::matching_commands(&first_line, &self.skills);
                    if !matches.is_empty() {
                        let popup_h = matches.len() as u16 + 2;
                        let popup_w = 40_u16.min(history_area.width.saturating_sub(4));
                        let popup_rect = Rect::new(
                            history_area.x + 2,
                            input_area.y.saturating_sub(popup_h),
                            popup_w,
                            popup_h,
                        );
                        let popup_lines: Vec<Line> = matches
                            .iter()
                            .map(|(name, desc)| {
                                Line::from(vec![
                                    Span::styled(
                                        format!(" {name:<12}"),
                                        Style::default()
                                            .fg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD),
                                    ),
                                    Span::styled(
                                        desc.to_string(),
                                        Style::default().fg(Color::DarkGray),
                                    ),
                                ])
                            })
                            .collect();
                        frame.render_widget(Clear, popup_rect);
                        frame.render_widget(
                            Paragraph::new(popup_lines).block(
                                Block::new()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::DarkGray)),
                            ),
                            popup_rect,
                        );
                    }
                }
            }
        }

        // ── provider model popup ──────────────────────────
        if let Some((ref alias, _)) = self.provider_models {
            let filtered = self.provider_model_filtered();
            if !filtered.is_empty() {
                let sel = self
                    .provider_model_sel
                    .min(filtered.len().saturating_sub(1));
                let popup_w = (history_area.width.saturating_sub(4)).min(70);
                let max_rows = history_area.height.saturating_sub(4).min(20) as usize;
                let popup_h = filtered.len().min(max_rows) as u16 + 2;
                let popup_rect =
                    Rect::new(history_area.x + 2, history_area.y + 1, popup_w, popup_h);
                // scroll window so selected row is visible
                let start = if sel >= max_rows {
                    sel - max_rows + 1
                } else {
                    0
                };
                let popup_lines: Vec<Line> = filtered[start..]
                    .iter()
                    .take(max_rows)
                    .enumerate()
                    .map(|(i, name)| {
                        let is_sel = start + i == sel;
                        let style = if is_sel {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::White)
                        };
                        Line::from(Span::styled(format!(" {name}"), style))
                    })
                    .collect();
                frame.render_widget(Clear, popup_rect);
                frame.render_widget(
                    Paragraph::new(popup_lines).block(
                        Block::new()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow))
                            .title(Span::styled(
                                format!(" {} ", alias),
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            )),
                    ),
                    popup_rect,
                );
            }
        }

        // ── approval card ─────────────────────────────────
        if let Some(ref approval) = self.pending_approval {
            let card_w = (area.width.saturating_sub(8)).min(60);
            let inner_w = card_w.saturating_sub(4) as usize;
            let args_lines = word_wrap(&approval.args_json, inner_w);
            // content rows: name + args + blank + keys; +2 for border
            let card_h = (5 + args_lines.len() as u16).min(area.height.saturating_sub(2));
            let card_x = (area.width.saturating_sub(card_w)) / 2;
            let card_y = (area.height.saturating_sub(card_h)) / 2;
            let card_rect = Rect::new(card_x, card_y, card_w, card_h);

            let color = if approval.is_dangerous {
                Color::Red
            } else {
                Color::Yellow
            };

            let mut card_lines = vec![Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    approval.name.clone(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ])];
            for l in &args_lines {
                card_lines.push(Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::Gray),
                )));
            }
            // separator + keybinding row
            card_lines.push(Line::raw(""));
            card_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    " y ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" approve    ", Style::default().fg(Color::Green)),
                Span::styled(
                    " n ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" deny", Style::default().fg(Color::Red)),
            ]));

            frame.render_widget(Clear, card_rect);
            frame.render_widget(
                Paragraph::new(card_lines).block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(color))
                        .title(Span::styled(
                            " tool call ",
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        )),
                ),
                card_rect,
            );
        }
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();

        for msg in &self.messages {
            let (label, label_style, text_style): (String, Style, Style) = match msg.role {
                Role::User => (
                    " you  ".into(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::White),
                ),
                Role::Assistant => {
                    let name = msg.model_name.as_deref().unwrap_or("ai");
                    (
                        format!(" {name}  "),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::White),
                    )
                }
                Role::System => (
                    " sys  ".into(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::DIM),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::DIM),
                ),
                Role::ToolCall => (
                    String::from(" tool "),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::DarkGray),
                ),
                Role::ToolResult => (
                    "      ".into(),
                    Style::default().fg(Color::DarkGray),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            };

            let label_len = label.len();
            let content_w = (self.view_width as usize).saturating_sub(label_len).max(1);
            let cont_pad = " ".repeat(label_len);

            if msg.role == Role::Assistant && !msg.content.is_empty() {
                // Render with markdown
                let md_lines = markdown_to_static_lines(&msg.content, content_w);
                for (i, line) in md_lines.into_iter().enumerate() {
                    if i == 0 {
                        let mut spans = vec![Span::styled(label.clone(), label_style)];
                        spans.extend(line.spans);
                        lines.push(Line::from(spans));
                    } else {
                        let mut spans = vec![Span::raw(cont_pad.clone())];
                        spans.extend(line.spans);
                        lines.push(Line::from(spans));
                    }
                }
            } else {
                let content = if msg.role == Role::ToolResult {
                    if let Some(ms) = msg.elapsed_ms {
                        format!("→ {} ({}ms)", msg.content, ms)
                    } else {
                        format!("→ {}", msg.content)
                    }
                } else {
                    msg.content.clone()
                };

                let wrapped = word_wrap(&content, content_w);
                for (i, chunk) in wrapped.into_iter().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(label.clone(), label_style),
                            Span::styled(chunk, text_style),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::raw(cont_pad.clone()),
                            Span::styled(chunk, text_style),
                        ]));
                    }
                }
            }
            lines.push(Line::raw(""));
        }

        let show_waiting = self.is_waiting
            && self.pending_approval.is_none()
            && self
                .messages
                .last()
                .map(|m| matches!(m.role, Role::User | Role::ToolResult | Role::ToolCall))
                .unwrap_or(true);
        if show_waiting {
            const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let spinner = SPINNER[(self.spinner_frame as usize / 3) % SPINNER.len()];
            lines.push(Line::from(Span::styled(
                format!(" {}  {}", self.current_model, spinner),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }

        lines
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
                self.user_tx.send(crate::ai::AgentCommand::Undo).ok();
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
            SlashCommandAction::ShowMessage(msg) | SlashCommandAction::Unknown(msg) => {
                self.push_system(msg);
            }
        }
    }

    fn push_system(&mut self, content: String) {
        self.messages.push(ChatMessage {
            role: Role::System,
            content,
            model_name: None,
            call_id: None,
            elapsed_ms: None,
        });
        self.auto_scroll = true;
    }

    fn submit_input(&mut self) {
        let input = self.textarea.lines().join("\n");
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        self.textarea = make_textarea(false);
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

    fn handle_events(&mut self) -> std::io::Result<()> {
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
                                self.textarea = make_textarea(false);
                            } else {
                                self.textarea = make_textarea(false);
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
                                    self.textarea = make_textarea_with(model_id);
                                }
                            } else {
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    let idx =
                                        self.model_ac_idx.unwrap_or(0).min(candidates.len() - 1);
                                    let alias = candidates[idx].alias.clone();
                                    self.textarea = make_textarea_with(format!("/model {alias}"));
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
                                            make_textarea_with(format!("/provider {name}"));
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
                                self.textarea = make_textarea(false);
                            } else {
                                let candidates = self.model_ac_candidates();
                                if !candidates.is_empty() {
                                    if let Some(idx) = self.model_ac_idx {
                                        let alias =
                                            candidates[idx.min(candidates.len() - 1)].alias.clone();
                                        self.textarea =
                                            make_textarea_with(format!("/model {alias}"));
                                    }
                                } else {
                                    let provider_names = self.provider_ac_candidates();
                                    if !provider_names.is_empty() {
                                        if let Some(idx) = self.provider_ac_idx {
                                            let name = provider_names
                                                [idx.min(provider_names.len() - 1)]
                                            .clone();
                                            self.textarea =
                                                make_textarea_with(format!("/provider {name}"));
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
                                        self.textarea = make_textarea_with(text);
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
                                            self.textarea = make_textarea_with(text);
                                        } else {
                                            self.history_cursor = None;
                                            self.textarea = make_textarea(false);
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

fn make_textarea(waiting: bool) -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta.set_placeholder_text("  ❯  message…  Shift+Enter for newline");
    ta.set_placeholder_style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    );
    if waiting {
        ta.set_style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        );
    }
    ta
}

fn make_textarea_with(text: String) -> TextArea<'static> {
    let mut ta = TextArea::new(vec![text]);
    ta.set_cursor_line_style(Style::default());
    ta.set_placeholder_text("  ❯  message…  Shift+Enter for newline");
    ta.set_placeholder_style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    );
    ta
}

fn markdown_to_static_lines(content: &str, content_w: usize) -> Vec<Line<'static>> {
    let text = tui_markdown::from_str(content);
    text.lines
        .into_iter()
        .flat_map(|line| {
            let owned = Line::from(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), s.style))
                    .collect::<Vec<_>>(),
            );
            wrap_styled_line(owned, content_w)
        })
        .collect()
}

/// Word-wraps a styled line to `max_width` columns, preserving each
/// character's style across the resulting lines.
fn wrap_styled_line(line: Line<'static>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 {
        return vec![line];
    }

    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();

    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur_word: Vec<(char, Style)> = Vec::new();
    for (c, style) in chars {
        if c.is_whitespace() {
            if !cur_word.is_empty() {
                words.push(std::mem::take(&mut cur_word));
            }
        } else {
            cur_word.push((c, style));
        }
    }
    if !cur_word.is_empty() {
        words.push(cur_word);
    }

    if words.is_empty() {
        return vec![Line::from(Vec::<Span<'static>>::new())];
    }

    let byte_width = |w: &[(char, Style)]| -> usize { w.iter().map(|(c, _)| c.len_utf8()).sum() };

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    for word in words {
        let word_w = byte_width(&word);
        if word_w > max_width {
            // Word alone is wider than the available space: hard-break it
            // so it can never overflow the terminal.
            if !cur.is_empty() {
                result.push(chars_to_line(std::mem::take(&mut cur)));
            }
            let mut chunk: Vec<(char, Style)> = Vec::new();
            let mut chunk_w = 0usize;
            for (c, style) in word {
                let c_w = c.len_utf8();
                if chunk_w + c_w > max_width && !chunk.is_empty() {
                    result.push(chars_to_line(std::mem::take(&mut chunk)));
                    chunk_w = 0;
                }
                chunk.push((c, style));
                chunk_w += c_w;
            }
            cur_w = chunk_w;
            cur = chunk;
        } else if cur.is_empty() {
            cur_w = word_w;
            cur = word;
        } else if cur_w + 1 + word_w <= max_width {
            cur.push((' ', Style::default()));
            cur.extend(word);
            cur_w += 1 + word_w;
        } else {
            result.push(chars_to_line(std::mem::take(&mut cur)));
            cur_w = word_w;
            cur = word;
        }
    }
    if !cur.is_empty() {
        result.push(chars_to_line(cur));
    }
    result
}

fn chars_to_line(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_str = String::new();
    let mut cur_style: Option<Style> = None;
    for (c, style) in chars {
        match cur_style {
            Some(s) if s == style => cur_str.push(c),
            Some(s) => {
                spans.push(Span::styled(std::mem::take(&mut cur_str), s));
                cur_str.push(c);
                cur_style = Some(style);
            }
            None => {
                cur_str.push(c);
                cur_style = Some(style);
            }
        }
    }
    if let Some(s) = cur_style {
        spans.push(Span::styled(cur_str, s));
    }
    Line::from(spans)
}

fn format_tool_call(name: &str, args_json: &str) -> String {
    // Pretty-print the args if possible
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) {
        if let Some(obj) = v.as_object() {
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}={val}")
                })
                .collect();
            return format!("{name}({})", parts.join(", "));
        }
    }
    format!("{name}({args_json})")
}

fn summarize_result(result: &str) -> String {
    let first_line = result.lines().next().unwrap_or("").trim();
    if first_line.len() > 80 {
        format!("{}…", &first_line[..77])
    } else {
        first_line.to_string()
    }
}

/// Largest byte index `<= index` that lies on a UTF-8 char boundary in `s`.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut result: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.trim().is_empty() {
            result.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if word.len() > max_width {
                // Word alone is wider than the available space: hard-break it
                // so it can never overflow the terminal.
                if !line.is_empty() {
                    result.push(std::mem::take(&mut line));
                }
                let mut rest = word;
                while rest.len() > max_width {
                    let split_at = floor_char_boundary(rest, max_width);
                    result.push(rest[..split_at].to_string());
                    rest = &rest[split_at..];
                }
                line = rest.to_string();
            } else if line.is_empty() {
                line = word.to_string();
            } else if line.len() + 1 + word.len() <= max_width {
                line.push(' ');
                line.push_str(word);
            } else {
                result.push(line);
                line = word.to_string();
            }
        }
        if !line.is_empty() {
            result.push(line);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}
