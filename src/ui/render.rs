//! Frame rendering: title/separator/history/input layout, chat-history line
//! building, autocomplete popups (model/provider/slash-command/provider-model),
//! and the tool-approval and checkpoint-confirmation cards.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::text::{markdown_to_static_lines, word_wrap};
use super::theme::Theme;
use super::{App, CheckpointPrompt, Role};

impl App {
    pub(super) fn draw(&mut self, frame: &mut Frame) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        let t = self.theme.clone();
        let area = frame.area();
        // Paint the theme background first; every widget below only sets a
        // foreground, so this shows through.
        frame.render_widget(Block::new().style(t.base()), area);
        let input_h = (self.textarea.lines().len() as u16).clamp(1, 5);
        // Update textarea style to reflect waiting state
        self.textarea.set_style(if self.is_waiting {
            Style::default().fg(t.subtle).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(t.text)
        });
        self.textarea
            .set_placeholder_style(Style::default().fg(t.subtle).add_modifier(Modifier::DIM));
        let [title_area, sep_top, history_area, sep_bot, input_area, status_area] =
            area.layout(&Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Length(input_h),
                Constraint::Length(1),
            ]));

        self.view_height = history_area.height;
        self.view_width = history_area.width;

        // ── title ─────────────────────────────────────────
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  magai",
                Style::default().fg(t.text).add_modifier(Modifier::BOLD),
            ))),
            title_area,
        );

        // ── separators ────────────────────────────────────
        let rule = Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(t.subtle),
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
        // The prompt is a fixed gutter rather than part of the placeholder:
        // the textarea draws its cursor cell before the placeholder text, which
        // put the cursor in front of the ❯, and the ❯ vanished once typing began.
        let [prompt_area, text_area] = input_area.layout(&Layout::horizontal([
            Constraint::Length(4),
            Constraint::Fill(1),
        ]));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  ❯ ",
                Style::default().fg(if self.is_waiting { t.subtle } else { t.accent }),
            ))),
            prompt_area,
        );
        frame.render_widget(&self.textarea, text_area);

        // ── model label (under the input) ─────────────────
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  [{}]", self.current_model),
                Style::default().fg(t.subtle),
            ))),
            status_area,
        );

        // ── command / model popup ─────────────────────────
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
                                .fg(t.on_accent)
                                .bg(t.accent)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                        };
                        let desc_style = if is_sel {
                            Style::default().fg(t.on_accent).bg(t.accent)
                        } else {
                            Style::default().fg(t.subtle)
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
                            .style(t.base())
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(t.subtle)),
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
                                    .fg(t.on_accent)
                                    .bg(t.accent)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                            };
                            Line::from(Span::styled(format!(" {name}"), style))
                        })
                        .collect();
                    frame.render_widget(Clear, popup_rect);
                    frame.render_widget(
                        Paragraph::new(popup_lines).block(
                            Block::new()
                                .style(t.base())
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(t.subtle)),
                        ),
                        popup_rect,
                    );
                } else {
                    let matches = self.cmd_ac_candidates();
                    if !matches.is_empty() {
                        let max_rows = history_area.height.saturating_sub(4).min(12) as usize;
                        let popup_h = matches.len().min(max_rows) as u16 + 2;
                        let popup_w = 40_u16.min(history_area.width.saturating_sub(4));
                        let popup_rect = Rect::new(
                            history_area.x + 2,
                            input_area.y.saturating_sub(popup_h),
                            popup_w,
                            popup_h,
                        );
                        // scroll the window so the selected row stays visible
                        let selected = self.cmd_ac_idx;
                        let start = match selected {
                            Some(i) if i >= max_rows => i - max_rows + 1,
                            _ => 0,
                        };
                        let popup_lines: Vec<Line> = matches[start..]
                            .iter()
                            .take(max_rows)
                            .enumerate()
                            .map(|(i, (name, desc))| {
                                let is_sel = selected == Some(start + i);
                                let (name_style, desc_style) = if is_sel {
                                    (
                                        Style::default()
                                            .fg(t.on_accent)
                                            .bg(t.accent)
                                            .add_modifier(Modifier::BOLD),
                                        Style::default().fg(t.on_accent).bg(t.accent),
                                    )
                                } else {
                                    (
                                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                                        Style::default().fg(t.subtle),
                                    )
                                };
                                Line::from(vec![
                                    Span::styled(format!(" {name:<12}"), name_style),
                                    Span::styled(desc.to_string(), desc_style),
                                ])
                            })
                            .collect();
                        frame.render_widget(Clear, popup_rect);
                        frame.render_widget(
                            Paragraph::new(popup_lines).block(
                                Block::new()
                                    .style(t.base())
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(t.subtle)),
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
                            Style::default().fg(t.on_accent).bg(t.accent)
                        } else {
                            Style::default().fg(t.text)
                        };
                        Line::from(Span::styled(format!(" {name}"), style))
                    })
                    .collect();
                frame.render_widget(Clear, popup_rect);
                frame.render_widget(
                    Paragraph::new(popup_lines).block(
                        Block::new()
                            .style(t.base())
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(t.accent))
                            .title(Span::styled(
                                format!(" {} ", alias),
                                Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                            )),
                    ),
                    popup_rect,
                );
            }
        }

        // ── checkpoint confirmation card ───────────────────
        if let Some(prompt) = &self.pending_checkpoint {
            render_checkpoint_prompt(frame, area, prompt, &t);
        }

        // ── max turns reached prompt card ───────────────────
        if let Some(max_turns) = self.pending_max_turns {
            let card_w = (area.width.saturating_sub(8)).min(58);
            let card_h = 7_u16;
            let card_x = (area.width.saturating_sub(card_w)) / 2;
            let card_y = (area.height.saturating_sub(card_h)) / 2;
            let card_rect = Rect::new(card_x, card_y, card_w, card_h);
            let shown = if self.max_turns_input.is_empty() {
                "10".to_string()
            } else {
                self.max_turns_input.clone()
            };
            let card_lines = vec![
                Line::from(Span::styled(
                    format!("  Reached {max_turns} turns without finishing."),
                    Style::default().fg(t.warning),
                )),
                Line::raw(""),
                Line::from(vec![
                    Span::raw("  Continue for "),
                    Span::styled(shown, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" more turns?"),
                ]),
                Line::raw(""),
                Line::from(vec![
                    Span::styled(" Enter ", Style::default().fg(t.on_accent).bg(t.success)),
                    Span::styled(" confirm   ", Style::default().fg(t.success)),
                    Span::styled(" Esc ", Style::default().fg(t.on_accent).bg(t.warning)),
                    Span::styled(" stop   ", Style::default().fg(t.warning)),
                    Span::raw("type digits to change amount"),
                ]),
            ];
            frame.render_widget(Clear, card_rect);
            frame.render_widget(
                Paragraph::new(card_lines).block(
                    Block::new()
                        .style(t.base())
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(t.warning))
                        .title(Span::styled(
                            " max turns reached ",
                            Style::default().fg(t.warning).add_modifier(Modifier::BOLD),
                        )),
                ),
                card_rect,
            );
        }

        // ── approval card ─────────────────────────────────
        if let Some(ref approval) = self.pending_approval {
            let card_w = (area.width.saturating_sub(8)).min(60);
            let inner_w = card_w.saturating_sub(4) as usize;
            let args_lines = word_wrap(&approval.args_json, inner_w);
            let note_lines = approval
                .review_note
                .as_deref()
                .map(|n| word_wrap(n, inner_w))
                .unwrap_or_default();
            // content rows: name + args + blank + keys (+ blank + note); +2 for border
            let note_h = if note_lines.is_empty() {
                0
            } else {
                1 + note_lines.len() as u16
            };
            let card_h = (5 + args_lines.len() as u16 + note_h).min(area.height.saturating_sub(2));
            let card_x = (area.width.saturating_sub(card_w)) / 2;
            let card_y = (area.height.saturating_sub(card_h)) / 2;
            let card_rect = Rect::new(card_x, card_y, card_w, card_h);

            let color = if approval.is_dangerous {
                t.danger
            } else {
                t.warning
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
                    Style::default().fg(t.muted),
                )));
            }
            if !note_lines.is_empty() {
                card_lines.push(Line::raw(""));
                for l in &note_lines {
                    card_lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(t.warning),
                    )));
                }
            }
            // separator + keybinding row
            card_lines.push(Line::raw(""));
            card_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    " y ",
                    Style::default()
                        .fg(t.on_accent)
                        .bg(t.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" approve    ", Style::default().fg(t.success)),
                Span::styled(
                    " n ",
                    Style::default()
                        .fg(t.on_accent)
                        .bg(t.danger)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" deny", Style::default().fg(t.danger)),
            ]));

            frame.render_widget(Clear, card_rect);
            frame.render_widget(
                Paragraph::new(card_lines).block(
                    Block::new()
                        .style(t.base())
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
        let t = &self.theme;
        let mut lines: Vec<Line<'static>> = Vec::new();

        for msg in &self.messages {
            let (label, label_style, text_style): (String, Style, Style) = match msg.role {
                Role::User => (
                    " you  ".into(),
                    Style::default().fg(t.user).add_modifier(Modifier::BOLD),
                    Style::default().fg(t.text),
                ),
                Role::Assistant => {
                    let name = msg.model_name.as_deref().unwrap_or("ai");
                    (
                        format!(" {name}  "),
                        Style::default()
                            .fg(t.assistant)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(t.text),
                    )
                }
                Role::System => (
                    " sys  ".into(),
                    Style::default().fg(t.system).add_modifier(Modifier::DIM),
                    Style::default().fg(t.system).add_modifier(Modifier::DIM),
                ),
                Role::Diff => (
                    "      ".into(),
                    Style::default().fg(t.subtle),
                    Style::default().fg(t.muted),
                ),
                Role::ToolCall => (
                    String::from(" tool "),
                    Style::default().fg(t.tool).add_modifier(Modifier::BOLD),
                    Style::default().fg(t.subtle),
                ),
                Role::ToolResult => (
                    "      ".into(),
                    Style::default().fg(t.subtle),
                    Style::default().fg(t.subtle).add_modifier(Modifier::DIM),
                ),
            };

            let label_len = label.len();
            let content_w = (self.view_width as usize).saturating_sub(label_len).max(1);
            let cont_pad = " ".repeat(label_len);

            if msg.role == Role::Diff {
                // Truncate rather than wrap: a wrapped patch is unreadable, and
                // the +/- column has to stay in place to mean anything.
                for (i, raw) in msg.content.lines().enumerate() {
                    let style = if raw.starts_with("+++") || raw.starts_with("---") {
                        Style::default().fg(t.subtle).add_modifier(Modifier::BOLD)
                    } else if raw.starts_with("diff ") || raw.starts_with("index ") {
                        Style::default().fg(t.subtle)
                    } else if raw.starts_with("@@") {
                        Style::default().fg(t.diff_hunk)
                    } else if raw.starts_with('+') {
                        Style::default().fg(t.diff_add)
                    } else if raw.starts_with('-') {
                        Style::default().fg(t.diff_remove)
                    } else {
                        text_style
                    };
                    let cut = crate::ui::text::floor_char_boundary(raw, content_w);
                    let prefix = if i == 0 {
                        Span::styled(label.clone(), label_style)
                    } else {
                        Span::raw(cont_pad.clone())
                    };
                    lines.push(Line::from(vec![
                        prefix,
                        Span::styled(raw[..cut].to_string(), style),
                    ]));
                }
                lines.push(Line::raw(""));
                continue;
            }

            if msg.role == Role::Assistant && !msg.content.is_empty() {
                // Render with markdown
                let md_lines = markdown_to_static_lines(&msg.content, content_w, t);
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
                Style::default().fg(t.subtle).add_modifier(Modifier::ITALIC),
            )));
        }

        lines
    }
}

/// The confirmation card shown before an undo or restore touches any file.
/// Height follows the content, since the stat block is variable-length.
fn render_checkpoint_prompt(frame: &mut Frame, area: Rect, prompt: &CheckpointPrompt, t: &Theme) {
    let card_w = (area.width.saturating_sub(8)).min(72);
    let inner_w = card_w.saturating_sub(4) as usize;

    let mut card_lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!("  {}", truncate(&prompt.title, inner_w)),
            Style::default().fg(t.info).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];

    for line in &prompt.lines {
        card_lines.push(Line::from(Span::styled(
            truncate(line, inner_w),
            Style::default().fg(t.muted),
        )));
    }
    card_lines.push(Line::raw(""));

    let (border, keys) = match &prompt.blocked {
        Some(reason) => {
            card_lines.insert(
                2,
                Line::from(Span::styled(
                    format!("  {}", truncate(reason, inner_w)),
                    Style::default().fg(t.danger),
                )),
            );
            card_lines.insert(3, Line::raw(""));
            (
                t.danger,
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        " esc ",
                        Style::default()
                            .fg(t.on_accent)
                            .bg(t.warning)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" dismiss", Style::default().fg(t.warning)),
                ]),
            )
        }
        None => (
            t.info,
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    " y ",
                    Style::default()
                        .fg(t.on_accent)
                        .bg(t.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" apply    ", Style::default().fg(t.success)),
                Span::styled(
                    " n ",
                    Style::default()
                        .fg(t.on_accent)
                        .bg(t.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" cancel", Style::default().fg(t.warning)),
            ]),
        ),
    };
    card_lines.push(keys);

    let card_h = (card_lines.len() as u16 + 2).min(area.height);
    let card_x = (area.width.saturating_sub(card_w)) / 2;
    let card_y = (area.height.saturating_sub(card_h)) / 2;
    let card_rect = Rect::new(card_x, card_y, card_w, card_h);

    frame.render_widget(Clear, card_rect);
    frame.render_widget(
        Paragraph::new(card_lines).block(
            Block::new()
                .style(t.base())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border))
                .title(Span::styled(
                    " checkpoint ",
                    Style::default().fg(border).add_modifier(Modifier::BOLD),
                )),
        ),
        card_rect,
    );
}

/// Width-safe truncation for card text, never splitting a codepoint.
fn truncate(s: &str, max: usize) -> String {
    let cut = crate::ui::text::floor_char_boundary(s, max);
    s[..cut].to_string()
}
