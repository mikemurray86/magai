//! Owns the ratatui event loop (`App::run`): the `App` state, the
//! `AiEvent`/`ChatMessage` types, and the bridge from `ai::run_agent` events
//! to on-screen state. See `render` for drawing, `input` for event handling,
//! and `text` for pure formatting/wrapping helpers.

mod input;
mod render;
mod text;

use ratatui::{
    style::{Color, Modifier, Style},
    DefaultTerminal,
};
use ratatui_textarea::TextArea;
use tokio::sync::mpsc;

use text::{format_tool_call, summarize_result};

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
    DirtyWorkspacePrompt,
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
    pending_dirty_workspace: bool,
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
            textarea: make_textarea("", false),
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
            pending_dirty_workspace: false,
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
                    self.textarea = make_textarea("", false);
                    self.model_ac_idx = None;
                }
                AiEvent::DirtyWorkspacePrompt => {
                    self.pending_dirty_workspace = true;
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
}

fn make_textarea(text: &str, waiting: bool) -> TextArea<'static> {
    let mut ta = if text.is_empty() {
        TextArea::default()
    } else {
        TextArea::new(vec![text.to_string()])
    };
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
