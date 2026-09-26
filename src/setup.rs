//! `magai init` and `/config`: an interactive walk through
//! `~/.config/magai/config.toml`.
//!
//! Every question shows the value currently in the file as its placeholder
//! (Enter keeps it) and the built-in default in the help line (`-` resets to
//! it by removing the key). Edits go through `toml_edit`, like `magai mcp`, so
//! comments and anything the wizard doesn't ask about survive untouched; a key
//! the user leaves at its default is not written at all.
//!
//! The questions talk to a [`Prompter`] rather than the terminal, so the
//! wizard is tested with scripted answers.

use std::io::IsTerminal;
use std::path::Path;

use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

use crate::config::{Config, AUTODETECT, OLLAMA_DEFAULT_BASE_URL};

const CANCELLED: &str = "setup cancelled — nothing was written";

/// The questions the wizard asks, independent of how they are shown.
pub trait Prompter {
    /// Free text. `current` is the placeholder; the returned string is what
    /// was typed, empty when the user just pressed Enter.
    fn text(&mut self, message: &str, current: Option<&str>, help: &str) -> Result<String, String>;
    /// Pick one of `options`, starting at `start`; returns the index.
    fn select(
        &mut self,
        message: &str,
        options: &[String],
        start: usize,
        help: &str,
    ) -> Result<usize, String>;
    fn confirm(&mut self, message: &str, default: bool, help: &str) -> Result<bool, String>;
    /// Pick any of `options`, with `checked` preselected.
    fn multi(
        &mut self,
        message: &str,
        options: &[String],
        checked: &[usize],
    ) -> Result<Vec<usize>, String>;
    /// A line of information between questions.
    fn note(&mut self, message: &str);
}

/// [`Prompter`] on the real terminal, via `inquire`.
pub struct Terminal;

fn inquire_err(e: inquire::InquireError) -> String {
    match e {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            CANCELLED.to_string()
        }
        e => e.to_string(),
    }
}

impl Prompter for Terminal {
    fn text(&mut self, message: &str, current: Option<&str>, help: &str) -> Result<String, String> {
        let mut prompt = inquire::Text::new(message).with_help_message(help);
        if let Some(c) = current {
            prompt = prompt.with_placeholder(c);
        }
        prompt.prompt().map_err(inquire_err)
    }

    fn select(
        &mut self,
        message: &str,
        options: &[String],
        start: usize,
        help: &str,
    ) -> Result<usize, String> {
        inquire::Select::new(message, options.to_vec())
            .with_starting_cursor(start)
            .with_help_message(help)
            .raw_prompt()
            .map(|o| o.index)
            .map_err(inquire_err)
    }

    fn confirm(&mut self, message: &str, default: bool, help: &str) -> Result<bool, String> {
        inquire::Confirm::new(message)
            .with_default(default)
            .with_help_message(help)
            .prompt()
            .map_err(inquire_err)
    }

    fn multi(
        &mut self,
        message: &str,
        options: &[String],
        checked: &[usize],
    ) -> Result<Vec<usize>, String> {
        inquire::MultiSelect::new(message, options.to_vec())
            .with_default(checked)
            .raw_prompt()
            .map(|os| os.into_iter().map(|o| o.index).collect())
            .map_err(inquire_err)
    }

    fn note(&mut self, message: &str) {
        println!("{message}");
    }
}

// ── entry points ─────────────────────────────────────────────────────────────

/// `magai init`, `/config` and the first-run offer: run the wizard on the
/// real config file and write it after confirmation. `Ok(None)` when nothing
/// changed or the user chose not to save.
pub fn run_interactive() -> Result<Option<String>, String> {
    if !std::io::stdin().is_terminal() {
        return Err("setup needs an interactive terminal".to_string());
    }
    let path = crate::config::config_path()
        .ok_or_else(|| "could not locate a config directory (is $HOME set?)".to_string())?;
    let existing = if path.exists() {
        Some(
            std::fs::read_to_string(&path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?,
        )
    } else {
        None
    };
    let mut ui = Terminal;
    match existing.as_deref() {
        Some(_) => ui.note(&format!(
            "Editing {} — Enter keeps the current value, '-' resets to the default.",
            path.display()
        )),
        None => ui.note(&format!("Creating {}.", path.display())),
    }
    let doc = run(&mut ui, existing.as_deref())?;
    save(&mut ui, &path, existing.as_deref(), &doc)
}

/// On a bare `magai` with no config file, offer to create one before the TUI
/// opens. Declining (or a non-interactive stdin) carries on as before, with
/// providers detected from the environment.
pub fn offer_first_run() {
    let Some(path) = crate::config::config_path() else {
        return;
    };
    if path.exists() || !std::io::stdin().is_terminal() {
        return;
    }
    let mut ui = Terminal;
    let help = "you can run `magai init` or /config later";
    match ui.confirm(
        &format!("No config found at {} — set one up now?", path.display()),
        true,
        help,
    ) {
        Ok(true) => {}
        _ => return,
    }
    match run_interactive() {
        Ok(Some(msg)) => println!("{msg}"),
        Ok(None) => {}
        Err(e) => eprintln!("{e}"),
    }
}

fn save(
    ui: &mut dyn Prompter,
    path: &Path,
    before: Option<&str>,
    doc: &DocumentMut,
) -> Result<Option<String>, String> {
    let after = doc.to_string();
    if before == Some(after.as_str()) || (before.is_none() && after.trim().is_empty()) {
        ui.note("No changes.");
        return Ok(None);
    }
    if !ui.confirm(&format!("Write {}?", path.display()), true, "")? {
        return Ok(None);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    std::fs::write(path, &after).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(Some(format!("Saved {}.", path.display())))
}

// ── the wizard ───────────────────────────────────────────────────────────────

const SECTIONS: &[&str] = &[
    "Providers",
    "Models",
    "General (default model, permissions mode, limits, theme)",
    "Permissions (safe commands/tools, smart reviewer)",
    "Memory, quality & checkpoints",
];

/// Walks the sections and returns the edited document, validated against
/// [`Config`]. `existing` is the current file's text, if there is one; every
/// section is asked on a first run, and only the chosen ones otherwise.
pub fn run(ui: &mut dyn Prompter, existing: Option<&str>) -> Result<DocumentMut, String> {
    let mut doc = match existing {
        Some(src) => src
            .parse::<DocumentMut>()
            .map_err(|e| format!("the config is not valid TOML, fix it first: {e}"))?,
        None => DocumentMut::new(),
    };
    let sections: Vec<usize> = if existing.is_some() {
        let all: Vec<usize> = (0..SECTIONS.len()).collect();
        let options: Vec<String> = SECTIONS.iter().map(|s| s.to_string()).collect();
        ui.multi("Which sections do you want to go through?", &options, &all)?
    } else {
        (0..SECTIONS.len()).collect()
    };

    let defaults: Config = toml::from_str("").expect("an empty config parses");
    for section in sections {
        match section {
            0 => providers(ui, &mut doc)?,
            1 => models(ui, &mut doc)?,
            2 => general(ui, &mut doc, &defaults)?,
            3 => permissions(ui, &mut doc)?,
            _ => background(ui, &mut doc, &defaults)?,
        }
    }

    toml::from_str::<Config>(&doc.to_string())
        .map_err(|e| format!("the result would not load, so nothing was written: {e}"))?;
    Ok(doc)
}

// ── document helpers ─────────────────────────────────────────────────────────

fn get<'t>(t: &'t Table, path: &[&str]) -> Option<&'t Item> {
    let (last, parents) = path.split_last()?;
    let mut cur = t;
    for key in parents {
        cur = cur.get(key)?.as_table()?;
    }
    cur.get(last).filter(|i| !i.is_none())
}

fn get_str<'t>(t: &'t Table, path: &[&str]) -> Option<&'t str> {
    get(t, path).and_then(Item::as_str)
}

/// The table at `path`, creating missing levels (implicit, so an otherwise
/// empty `[permissions]` gets no header of its own).
fn table_mut<'t>(t: &'t mut Table, path: &[&str]) -> Result<&'t mut Table, String> {
    let mut cur = t;
    for key in path {
        let item = cur.entry(key).or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        });
        cur = item
            .as_table_mut()
            .ok_or_else(|| format!("`{key}` in the config is not a [table]; edit it by hand"))?;
    }
    Ok(cur)
}

fn set(t: &mut Table, path: &[&str], item: Item) -> Result<(), String> {
    let (last, parents) = path.split_last().expect("non-empty path");
    table_mut(t, parents)?.insert(last, item);
    Ok(())
}

fn unset(t: &mut Table, path: &[&str]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = t;
    for key in parents {
        match cur.get_mut(key).and_then(Item::as_table_mut) {
            Some(next) => cur = next,
            None => return,
        }
    }
    cur.remove(last);
}

/// How an item reads back as a placeholder.
fn show(item: &Item) -> String {
    use toml_edit::Value;
    // Match on the parsed value, never `to_string()`, which keeps the
    // surrounding whitespace and any trailing `# comment`.
    fn scalar(v: &Value) -> String {
        match v {
            Value::String(s) => s.value().clone(),
            Value::Integer(i) => i.value().to_string(),
            Value::Float(f) => f.value().to_string(),
            Value::Boolean(b) => b.value().to_string(),
            other => other.clone().decorated("", "").to_string(),
        }
    }
    match item.as_value() {
        Some(Value::Array(a)) => a.iter().map(scalar).collect::<Vec<_>>().join(", "),
        Some(v) => scalar(v),
        None => String::new(),
    }
}

fn help(default: Option<&str>, has_current: bool) -> String {
    let default = format!("default: {}", default.unwrap_or("none"));
    if has_current {
        format!("{default} · Enter keeps current · '-' resets to default")
    } else {
        format!("{default} · Enter accepts it")
    }
}

// ── question kinds ───────────────────────────────────────────────────────────

/// A string setting. With `write_default`, a blank answer on an unset key
/// writes `default` (for keys with no serde default, like `api_key_env`).
fn ask_text(
    ui: &mut dyn Prompter,
    t: &mut Table,
    path: &[&str],
    message: &str,
    default: Option<&str>,
    write_default: bool,
) -> Result<(), String> {
    let current = get(t, path).map(show);
    let answer = ui.text(
        message,
        current.as_deref(),
        &help(default, current.is_some()),
    )?;
    match answer.trim() {
        "" if current.is_none() && write_default => {
            if let Some(d) = default {
                set(t, path, value(d))?;
            }
        }
        "" => {}
        "-" => unset(t, path),
        s if Some(s) == default && current.is_none() && !write_default => {}
        s => set(t, path, value(s))?,
    }
    Ok(())
}

/// A string that must end up non-empty (a provider name, a model id).
fn ask_required(
    ui: &mut dyn Prompter,
    message: &str,
    current: Option<&str>,
    valid: impl Fn(&str) -> Result<(), String>,
) -> Result<String, String> {
    loop {
        let help = if current.is_some() {
            "required · Enter keeps current"
        } else {
            "required"
        };
        let answer = ui.text(message, current, help)?;
        let answer = match (answer.trim(), current) {
            ("", Some(c)) => return Ok(c.to_string()),
            ("", None) => {
                ui.note("  a value is required");
                continue;
            }
            (s, _) => s.to_string(),
        };
        match valid(&answer) {
            Ok(()) => return Ok(answer),
            Err(e) => ui.note(&format!("  {e}")),
        }
    }
}

fn ask_number(
    ui: &mut dyn Prompter,
    t: &mut Table,
    path: &[&str],
    message: &str,
    default: usize,
) -> Result<(), String> {
    let current = get(t, path).map(show);
    let default_s = default.to_string();
    loop {
        let answer = ui.text(
            message,
            current.as_deref(),
            &help(Some(&default_s), current.is_some()),
        )?;
        match answer.trim() {
            "" => return Ok(()),
            "-" => {
                unset(t, path);
                return Ok(());
            }
            s => match s.replace('_', "").parse::<usize>() {
                Ok(n) if n == default && current.is_none() => return Ok(()),
                Ok(n) if n > 0 => return set(t, path, value(n as i64)),
                _ => ui.note("  enter a positive whole number"),
            },
        }
    }
}

fn ask_bool(
    ui: &mut dyn Prompter,
    t: &mut Table,
    path: &[&str],
    message: &str,
    default: bool,
) -> Result<bool, String> {
    let current = get(t, path).and_then(Item::as_bool);
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let answer = ui.confirm(
        message,
        current.unwrap_or(default),
        &format!("default: {}", yes_no(default)),
    )?;
    if Some(answer) != current && !(current.is_none() && answer == default) {
        set(t, path, value(answer))?;
    }
    Ok(answer)
}

/// Pick one of `choices`. `default` is labelled; with `optional`, a
/// "(none)" entry unsets the key. Picking the default on an unset key leaves
/// it unset. Returns the effective value.
fn ask_choice(
    ui: &mut dyn Prompter,
    t: &mut Table,
    path: &[&str],
    message: &str,
    choices: &[String],
    default: Option<&str>,
    optional: bool,
) -> Result<Option<String>, String> {
    let current = get_str(t, path).map(str::to_owned);
    let mut values: Vec<Option<String>> = choices.iter().cloned().map(Some).collect();
    if let Some(c) = &current {
        if !choices.contains(c) {
            values.insert(0, Some(c.clone()));
        }
    }
    if optional {
        values.push(None);
    }
    let labels: Vec<String> = values
        .iter()
        .map(|v| {
            let mut label = v.clone().unwrap_or_else(|| "(none)".to_string());
            if v.as_deref() == default {
                label.push_str("  (default)");
            }
            if *v == current {
                label.push_str("  (current)");
            }
            label
        })
        .collect();
    let start = values
        .iter()
        .position(|v| *v == current)
        .or_else(|| values.iter().position(|v| v.as_deref() == default))
        .unwrap_or(0);
    let help = format!("default: {}", default.unwrap_or("none"));
    let picked = values[ui.select(message, &labels, start, &help)?].clone();
    match &picked {
        None => unset(t, path),
        Some(v) if current.is_none() && Some(v.as_str()) == default => {}
        Some(v) if Some(v) == current.as_ref() => {}
        Some(v) => set(t, path, value(v.as_str()))?,
    }
    Ok(picked.or_else(|| default.map(str::to_owned)))
}

/// A list of strings, asked as one comma-separated line.
fn ask_list(
    ui: &mut dyn Prompter,
    t: &mut Table,
    path: &[&str],
    message: &str,
) -> Result<(), String> {
    let current = get(t, path).map(show).filter(|s| !s.is_empty());
    let help = if current.is_some() {
        "comma-separated · default: none · Enter keeps current · '-' clears"
    } else {
        "comma-separated · default: none"
    };
    let answer = ui.text(message, current.as_deref(), help)?;
    match answer.trim() {
        "" => {}
        "-" => unset(t, path),
        s => {
            let mut arr = Array::new();
            for item in s.split(',').map(str::trim).filter(|i| !i.is_empty()) {
                arr.push(item);
            }
            set(t, path, value(arr))?;
        }
    }
    Ok(())
}

/// `add`/`edit`/`remove`/`done` over a list of named entries.
enum Action {
    Edit(usize),
    Add,
    Remove,
    Done,
}

fn pick_action(ui: &mut dyn Prompter, message: &str, entries: &[String]) -> Result<Action, String> {
    let mut options: Vec<String> = entries.iter().map(|e| format!("Edit {e}")).collect();
    options.push("Add".to_string());
    if !entries.is_empty() {
        options.push("Remove".to_string());
    }
    options.push("Done".to_string());
    let done = options.len() - 1;
    let i = ui.select(message, &options, done, "")?;
    Ok(if i < entries.len() {
        Action::Edit(i)
    } else if i == entries.len() {
        Action::Add
    } else if i == done {
        Action::Done
    } else {
        Action::Remove
    })
}

// ── sections ─────────────────────────────────────────────────────────────────

const PROVIDER_TYPES: &[&str] = &["ollama", "openai", "anthropic", "groq"];

fn provider_names(doc: &DocumentMut) -> Vec<String> {
    doc.get("providers")
        .and_then(Item::as_table)
        .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default()
}

fn providers(ui: &mut dyn Prompter, doc: &mut DocumentMut) -> Result<(), String> {
    ui.note("\n── Providers ── where models are served from");
    loop {
        let names = provider_names(doc);
        let labels: Vec<String> = names
            .iter()
            .map(|n| {
                let ty = get_str(doc, &["providers", n, "type"]).unwrap_or("?");
                format!("{n} ({ty})")
            })
            .collect();
        let action = if names.is_empty() {
            Action::Add
        } else {
            pick_action(ui, "Providers:", &labels)?
        };
        match action {
            Action::Done => return Ok(()),
            Action::Edit(i) => edit_provider(ui, doc, &names[i])?,
            Action::Add => {
                let name =
                    ask_required(ui, "Provider name (e.g. openrouter, local):", None, |s| {
                        if names.iter().any(|n| n == s) {
                            Err(format!("{s:?} already exists"))
                        } else if s.contains(['.', ' ', '"']) {
                            Err("use letters, digits, - and _".to_string())
                        } else {
                            Ok(())
                        }
                    })?;
                edit_provider(ui, doc, &name)?;
                if names.is_empty() && !ui.confirm("Add another provider?", false, "")? {
                    return Ok(());
                }
            }
            Action::Remove => {
                let i = ui.select("Remove which provider?", &labels, 0, "")?;
                if let Some(t) = doc.get_mut("providers").and_then(Item::as_table_mut) {
                    t.remove(&names[i]);
                }
            }
        }
    }
}

fn edit_provider(ui: &mut dyn Prompter, doc: &mut DocumentMut, name: &str) -> Result<(), String> {
    let t = table_mut(doc, &["providers", name])?;
    t.set_implicit(false);
    let types: Vec<String> = PROVIDER_TYPES.iter().map(|s| s.to_string()).collect();
    // `type` is required, so pick the first entry rather than leave it unset.
    let ty = ask_choice(ui, t, &["type"], "Type:", &types, None, false)?
        .expect("a required choice has a value");

    if ty == "ollama" {
        ask_text(
            ui,
            t,
            &["base_url"],
            "Base URL:",
            Some(OLLAMA_DEFAULT_BASE_URL),
            false,
        )?;
        return Ok(());
    }
    let suggested_env = AUTODETECT
        .iter()
        .find(|(p, _)| p.as_str() == ty)
        .map(|(_, env)| *env);
    ask_text(
        ui,
        t,
        &["api_key_env"],
        "Environment variable holding the API key:",
        suggested_env,
        true,
    )?;
    if let Some(var) = get_str(t, &["api_key_env"]) {
        if std::env::var_os(var).is_none() {
            ui.note(&format!("  note: ${var} is not set in this shell"));
        }
    }
    ask_text(
        ui,
        t,
        &["base_url"],
        "Base URL (for proxies/compatible servers):",
        None,
        false,
    )?;
    if ty == "openai" {
        let apis = vec!["chat".to_string(), "responses".to_string()];
        ask_choice(ui, t, &["api"], "Wire API:", &apis, Some("chat"), false)?;
    }
    Ok(())
}

fn models_mut(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables, String> {
    doc.entry("named_models")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| "`named_models` in the config is not a list of [[named_models]]".to_string())
}

fn aliases(doc: &DocumentMut) -> Vec<String> {
    doc.get("named_models")
        .and_then(Item::as_array_of_tables)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("alias").and_then(Item::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn models(ui: &mut dyn Prompter, doc: &mut DocumentMut) -> Result<(), String> {
    ui.note("\n── Models ── aliases you switch between with /model");
    let providers = provider_names(doc);
    if providers.is_empty() {
        ui.note("  no providers yet — add one first; skipping models");
        return Ok(());
    }
    loop {
        let entries: Vec<(String, String)> = models_mut(doc)?
            .iter()
            .map(|t| {
                let s = |k| t.get(k).and_then(Item::as_str).unwrap_or("?").to_string();
                (s("alias"), format!("{}/{}", s("provider"), s("model")))
            })
            .collect();
        let labels: Vec<String> = entries.iter().map(|(a, m)| format!("{a} ({m})")).collect();
        let action = if entries.is_empty() {
            Action::Add
        } else {
            pick_action(ui, "Named models:", &labels)?
        };
        match action {
            Action::Done => break,
            Action::Edit(i) => {
                let mut t = models_mut(doc)?.get(i).expect("listed").clone();
                edit_model(ui, doc, &mut t, &entries, Some(i))?;
                *models_mut(doc)?.get_mut(i).expect("listed") = t;
            }
            Action::Add => {
                let mut t = Table::new();
                edit_model(ui, doc, &mut t, &entries, None)?;
                models_mut(doc)?.push(t);
                if entries.is_empty() && !ui.confirm("Add another model?", false, "")? {
                    break;
                }
            }
            Action::Remove => {
                let i = ui.select("Remove which model?", &labels, 0, "")?;
                models_mut(doc)?.remove(i);
            }
        }
    }
    if models_mut(doc)?.is_empty() {
        doc.remove("named_models");
    }
    Ok(())
}

fn edit_model(
    ui: &mut dyn Prompter,
    doc: &DocumentMut,
    t: &mut Table,
    others: &[(String, String)],
    this: Option<usize>,
) -> Result<(), String> {
    let alias = ask_required(ui, "Alias:", get_str(t, &["alias"]), |s| {
        let taken = others
            .iter()
            .enumerate()
            .any(|(i, (a, _))| a == s && Some(i) != this);
        if taken {
            Err(format!("{s:?} is already used"))
        } else {
            Ok(())
        }
    })?;
    set(t, &["alias"], value(alias))?;

    let providers = provider_names(doc);
    let provider = ask_choice(ui, t, &["provider"], "Provider:", &providers, None, false)?
        .expect("a required choice has a value");

    let model = ask_required(ui, "Model id:", get_str(t, &["model"]), |_| Ok(()))?;
    set(t, &["model"], value(model))?;

    if get_str(doc, &["providers", &provider, "type"]) == Some("openai") {
        let apis = vec!["chat".to_string(), "responses".to_string()];
        ask_choice(
            ui,
            t,
            &["api"],
            "Wire API override (none = the provider's):",
            &apis,
            None,
            true,
        )?;
    }
    Ok(())
}

fn general(ui: &mut dyn Prompter, doc: &mut DocumentMut, d: &Config) -> Result<(), String> {
    ui.note("\n── General ──");
    let names = aliases(doc);
    if names.is_empty() {
        ask_text(
            ui,
            doc,
            &["default_model"],
            "Default model (a named model alias or an Ollama tag):",
            None,
            false,
        )?;
    } else {
        ask_choice(
            ui,
            doc,
            &["default_model"],
            "Default model:",
            &names,
            None,
            true,
        )?;
    }
    let modes: Vec<String> = ["ask-dangerous", "smart", "ask-always", "auto"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    ask_choice(
        ui,
        doc,
        &["permission_mode"],
        "Permission mode:",
        &modes,
        Some("ask-dangerous"),
        false,
    )?;
    ask_number(
        ui,
        doc,
        &["max_context_tokens"],
        "Max context tokens:",
        d.max_context_tokens,
    )?;
    ask_number(
        ui,
        doc,
        &["max_turns"],
        "Max turns per message:",
        d.max_turns,
    )?;

    let mut themes: Vec<String> = crate::ui::theme::BUILTIN_THEMES
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(custom) = doc.get("themes").and_then(Item::as_table) {
        themes.extend(custom.iter().map(|(k, _)| k.to_string()));
    }
    ask_choice(
        ui,
        doc,
        &["theme"],
        "Theme:",
        &themes,
        Some(crate::ui::theme::DEFAULT_THEME),
        false,
    )?;
    Ok(())
}

fn permissions(ui: &mut dyn Prompter, doc: &mut DocumentMut) -> Result<(), String> {
    ui.note("\n── Permissions ──");
    ask_list(
        ui,
        doc,
        &["permissions", "safe_commands"],
        "Shell commands that never need approval (prefixes, e.g. make test):",
    )?;
    ask_list(
        ui,
        doc,
        &["permissions", "safe_tools"],
        "Tools that never need approval:",
    )?;
    if get_str(doc, &["permission_mode"]) == Some("smart") {
        helper_model(
            ui,
            doc,
            &["permissions", "reviewer", "model"],
            None,
            "Reviewer model for smart mode:",
        )?;
        if get(doc, &["permissions", "reviewer", "model"]).is_none() {
            ui.note("  note: without a reviewer, smart mode behaves as ask-dangerous");
        }
    }
    Ok(())
}

/// The model for a background helper. `legacy` is the older flat key that
/// still counts as its current value; it is dropped once the new key is set.
fn helper_model(
    ui: &mut dyn Prompter,
    doc: &mut DocumentMut,
    path: &[&str],
    legacy: Option<&[&str]>,
    message: &str,
) -> Result<(), String> {
    if let Some(old) = legacy {
        if get(doc, path).is_none() {
            if let Some(v) = get_str(doc, old).map(str::to_owned) {
                set(doc, path, value(v))?;
            }
        }
        unset(doc, old);
    }
    let names = aliases(doc);
    if names.is_empty() {
        ask_text(ui, doc, path, message, None, false)
    } else {
        ask_choice(ui, doc, path, message, &names, None, true).map(|_| ())
    }
}

fn background(ui: &mut dyn Prompter, doc: &mut DocumentMut, d: &Config) -> Result<(), String> {
    ui.note("\n── Memory, quality & checkpoints ──");
    if ask_bool(
        ui,
        doc,
        &["memory", "enabled"],
        "Enable persistent memory?",
        d.memory.enabled,
    )? {
        ask_bool(
            ui,
            doc,
            &["memory", "inject_context"],
            "Inject relevant memories into each prompt?",
            d.memory.inject_context,
        )?;
        helper_model(
            ui,
            doc,
            &["memory", "extractor", "model"],
            Some(&["memory", "extract_facts_model"]),
            "Fact extractor model (none = off):",
        )?;
        if ask_bool(
            ui,
            doc,
            &["quality", "enabled"],
            "Record turns for quality tracking?",
            d.quality.enabled,
        )? {
            helper_model(
                ui,
                doc,
                &["quality", "judge", "model"],
                Some(&["quality", "judge_model"]),
                "Quality judge model (none = off):",
            )?;
        }
    }
    ask_bool(
        ui,
        doc,
        &["checkpoints", "enabled"],
        "Snapshot the working tree each turn (/undo, /restore)?",
        d.checkpoints.enabled,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Answers questions from a script, failing loudly on anything unexpected.
    #[derive(Default)]
    struct Script {
        answers: VecDeque<Answer>,
        asked: Vec<(String, Option<String>, String)>,
    }

    #[derive(Debug)]
    enum Answer {
        Text(&'static str),
        Pick(&'static str),
        Yes(bool),
        Multi(Vec<usize>),
    }

    impl Script {
        fn new(answers: Vec<Answer>) -> Self {
            Self {
                answers: answers.into(),
                asked: Vec::new(),
            }
        }
        fn next(&mut self, message: &str) -> Answer {
            self.answers
                .pop_front()
                .unwrap_or_else(|| panic!("unscripted question: {message}"))
        }
    }

    impl Prompter for Script {
        fn text(&mut self, m: &str, current: Option<&str>, help: &str) -> Result<String, String> {
            self.asked
                .push((m.to_string(), current.map(str::to_owned), help.to_string()));
            match self.next(m) {
                Answer::Text(s) => Ok(s.to_string()),
                a => panic!("{m}: expected text, scripted {a:?}"),
            }
        }
        fn select(
            &mut self,
            m: &str,
            options: &[String],
            _: usize,
            _: &str,
        ) -> Result<usize, String> {
            match self.next(m) {
                Answer::Pick(p) => Ok(options
                    .iter()
                    .position(|o| o.starts_with(p))
                    .unwrap_or_else(|| panic!("{m}: no option {p:?} in {options:?}"))),
                a => panic!("{m}: expected pick, scripted {a:?}"),
            }
        }
        fn confirm(&mut self, m: &str, _: bool, _: &str) -> Result<bool, String> {
            match self.next(m) {
                Answer::Yes(b) => Ok(b),
                a => panic!("{m}: expected yes/no, scripted {a:?}"),
            }
        }
        fn multi(&mut self, m: &str, _: &[String], _: &[usize]) -> Result<Vec<usize>, String> {
            match self.next(m) {
                Answer::Multi(v) => Ok(v),
                a => panic!("{m}: expected multi, scripted {a:?}"),
            }
        }
        fn note(&mut self, _: &str) {}
    }

    use Answer::*;

    #[test]
    fn first_run_builds_a_loadable_config() {
        let mut ui = Script::new(vec![
            // providers
            Text("hosted"),
            Pick("anthropic"),
            Text(""), // api_key_env: accept suggested ANTHROPIC_API_KEY
            Text(""), // base_url
            Yes(false),
            // models
            Text("claude"),
            Pick("hosted"),
            Text("claude-sonnet-5"),
            Yes(false),
            // general
            Pick("claude"),
            Pick("smart"),
            Text(""),
            Text("40"),
            Pick("catppuccin-mocha"),
            // permissions
            Text("make test, npm run lint"),
            Text(""),
            Pick("claude"),
            // memory etc: all defaults
            Yes(true),
            Yes(true),
            Pick("(none)"),
            Yes(false),
            Yes(true),
        ]);
        let doc = run(&mut ui, None).unwrap();
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();

        let p = &cfg.providers["hosted"];
        assert_eq!(p.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(cfg.named_models[0].model, "claude-sonnet-5");
        assert_eq!(cfg.default_model.as_deref(), Some("claude"));
        assert_eq!(cfg.max_turns, 40);
        assert_eq!(
            cfg.permissions.safe_commands,
            vec!["make test", "npm run lint"]
        );
        assert_eq!(cfg.permissions.reviewer.model.as_deref(), Some("claude"));

        // Defaults picked on unset keys are not written.
        let text = doc.to_string();
        for key in ["max_context_tokens", "theme", "enabled", "inject_context"] {
            assert!(!text.contains(key), "{key} should stay unset:\n{text}");
        }
    }

    #[test]
    fn existing_values_are_placeholders_and_comments_survive() {
        let src =
            "# my settings\nmax_turns = 50 # tuned\n\n[memory]\nextract_facts_model = \"local\"\n";
        let mut ui = Script::new(vec![
            Multi(vec![2, 4]), // general + memory
            Text("granite"),
            Pick("ask-dangerous"),
            Text(""),
            Text(""), // max_turns: keep 50
            Pick("classic"),
            Yes(true),
            Yes(true),
            Text(""), // extractor: keep migrated legacy value
            Yes(false),
            Yes(true),
        ]);
        let doc = run(&mut ui, Some(src)).unwrap();
        let text = doc.to_string();

        let (_, current, help) = ui
            .asked
            .iter()
            .find(|(m, ..)| m.starts_with("Max turns"))
            .unwrap();
        assert_eq!(current.as_deref(), Some("50"));
        assert!(help.contains("default: 25"), "{help}");

        assert!(
            text.contains("# my settings") && text.contains("# tuned"),
            "{text}"
        );
        assert!(text.contains("theme = \"classic\""));
        assert!(
            !text.contains("extract_facts_model"),
            "legacy key migrated:\n{text}"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.memory.extractor.model.as_deref(), Some("local"));
        assert_eq!(cfg.default_model.as_deref(), Some("granite"));
    }

    #[test]
    fn dash_resets_to_default() {
        let mut t = DocumentMut::new();
        t["max_turns"] = value(10);
        let mut ui = Script::new(vec![Text("-")]);
        ask_number(&mut ui, &mut t, &["max_turns"], "Max turns:", 25).unwrap();
        assert!(t.get("max_turns").is_none());
    }

    #[test]
    fn invalid_numbers_are_asked_again() {
        let mut t = DocumentMut::new();
        let mut ui = Script::new(vec![Text("lots"), Text("0"), Text("12")]);
        ask_number(&mut ui, &mut t, &["max_turns"], "Max turns:", 25).unwrap();
        assert_eq!(t["max_turns"].as_integer(), Some(12));
    }

    #[test]
    fn a_broken_file_is_refused_rather_than_overwritten() {
        let mut ui = Script::new(vec![]);
        assert!(run(&mut ui, Some("max_turns = [")).is_err());
    }
}
