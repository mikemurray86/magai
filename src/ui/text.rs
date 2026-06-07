//! Pure text-formatting helpers used to render chat history: markdown
//! conversion, style-preserving word wrap, plain word wrap, and tool
//! call/result summaries. Free functions with no `App` dependency — the
//! most directly testable code in the UI layer.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

pub(super) fn markdown_to_static_lines(content: &str, content_w: usize) -> Vec<Line<'static>> {
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

pub(super) fn format_tool_call(name: &str, args_json: &str) -> String {
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

pub(super) fn summarize_result(result: &str) -> String {
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

pub(super) fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
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
