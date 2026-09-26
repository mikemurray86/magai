//! Colour themes for the TUI. A `Theme` is a flat set of semantic colours
//! (what a colour *means* — "danger", "user label" — not what it is), so
//! render code never names a concrete colour. Built-ins live here; config can
//! define more under `[themes.<name>]`, each starting from a built-in `base`
//! and overriding individual keys. Resolution is a pure function so it can be
//! tested without a terminal.

use std::collections::HashMap;
use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};

/// Used when config names no theme.
pub const DEFAULT_THEME: &str = "catppuccin-mocha";

/// Built-in theme names, in the order `/theme` lists them.
pub const BUILTIN_THEMES: &[&str] = &["catppuccin-mocha", "classic"];

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    /// Painted behind everything; `Color::Reset` keeps the terminal's own.
    pub background: Color,
    pub text: Color,
    /// Secondary text: tool-call args, card bodies, diff context.
    pub muted: Color,
    /// De-emphasised chrome: rules, popup borders, the model label, tool output.
    pub subtle: Color,
    pub user: Color,
    pub assistant: Color,
    pub system: Color,
    pub tool: Color,
    /// Autocomplete names, the selected-row background, and focused popups.
    pub accent: Color,
    /// Foreground for text drawn on a filled accent/success/warning/danger badge.
    pub on_accent: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    pub diff_hunk: Color,
    /// Markdown `#` heading: foreground and background (either may be `Reset`).
    pub heading1: Color,
    pub heading1_bg: Color,
    /// Markdown `##`/`###`.
    pub heading: Color,
    /// Markdown `####` and deeper.
    pub minor_heading: Color,
    pub code: Color,
    pub code_bg: Color,
    pub link: Color,
    pub quote: Color,
}

impl Theme {
    /// The original 16-colour palette, kept byte-for-byte.
    pub fn classic() -> Self {
        Self {
            name: "classic".into(),
            background: Color::Reset,
            text: Color::White,
            muted: Color::Gray,
            subtle: Color::DarkGray,
            user: Color::Cyan,
            assistant: Color::Green,
            system: Color::Yellow,
            tool: Color::Magenta,
            accent: Color::Yellow,
            on_accent: Color::Black,
            success: Color::Green,
            warning: Color::Yellow,
            danger: Color::Red,
            info: Color::Cyan,
            diff_add: Color::Green,
            diff_remove: Color::Red,
            diff_hunk: Color::Cyan,
            heading1: Color::Reset,
            heading1_bg: Color::Cyan,
            heading: Color::Cyan,
            minor_heading: Color::LightCyan,
            code: Color::White,
            code_bg: Color::Black,
            link: Color::Blue,
            quote: Color::Green,
        }
    }

    /// https://catppuccin.com/palette — Mocha flavour.
    pub fn catppuccin_mocha() -> Self {
        const fn hex(v: u32) -> Color {
            Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
        }
        let red = hex(0xf38ba8);
        let peach = hex(0xfab387);
        let yellow = hex(0xf9e2af);
        let green = hex(0xa6e3a1);
        let teal = hex(0x94e2d5);
        let sapphire = hex(0x74c7ec);
        let blue = hex(0x89b4fa);
        let lavender = hex(0xb4befe);
        let mauve = hex(0xcba6f7);
        let text = hex(0xcdd6f4);
        let subtext0 = hex(0xa6adc8);
        let overlay2 = hex(0x9399b2);
        let overlay0 = hex(0x6c7086);
        let surface0 = hex(0x313244);
        let crust = hex(0x11111b);
        Self {
            name: "catppuccin-mocha".into(),
            // Terminal default rather than Mocha base (#1e1e2e), so a
            // translucent terminal background shows through.
            background: Color::Reset,
            text,
            muted: subtext0,
            subtle: overlay0,
            user: blue,
            assistant: green,
            system: yellow,
            tool: mauve,
            accent: mauve,
            on_accent: crust,
            success: green,
            warning: peach,
            danger: red,
            info: sapphire,
            diff_add: green,
            diff_remove: red,
            diff_hunk: sapphire,
            heading1: mauve,
            heading1_bg: Color::Reset,
            heading: lavender,
            minor_heading: teal,
            code: peach,
            code_bg: surface0,
            link: blue,
            quote: overlay2,
        }
    }

    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "catppuccin-mocha" => Some(Self::catppuccin_mocha()),
            "classic" => Some(Self::classic()),
            _ => None,
        }
    }

    /// Resolves `name` against the built-ins and the config's `[themes.*]`
    /// tables. Always yields a usable theme: an unknown name falls back to
    /// the default, and bad keys are skipped. Anything skipped is described
    /// in the returned warnings so the TUI can show it.
    pub fn resolve(
        name: &str,
        custom: &HashMap<String, HashMap<String, String>>,
    ) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let Some(table) = custom.get(name) else {
            if let Some(theme) = Self::builtin(name) {
                return (theme, warnings);
            }
            warnings.push(format!(
                "unknown theme {name:?}; using {DEFAULT_THEME} (available: {})",
                available_themes(custom).join(", ")
            ));
            return (Self::catppuccin_mocha(), warnings);
        };

        // `[themes.classic]` with no `base` tweaks classic, not the default.
        let fallback_base = if Self::builtin(name).is_some() {
            name
        } else {
            DEFAULT_THEME
        };
        let base = table
            .get("base")
            .map(String::as_str)
            .unwrap_or(fallback_base);
        let mut theme = Self::builtin(base).unwrap_or_else(|| {
            warnings.push(format!(
                "theme {name:?}: unknown base {base:?}; using {DEFAULT_THEME}"
            ));
            Self::catppuccin_mocha()
        });
        theme.name = name.to_string();

        let mut keys: Vec<_> = table.iter().filter(|(k, _)| *k != "base").collect();
        keys.sort();
        for (key, value) in keys {
            let Some(slot) = theme.slot_mut(key) else {
                warnings.push(format!("theme {name:?}: unknown key {key:?}"));
                continue;
            };
            match parse_color(value) {
                Some(c) => *slot = c,
                None => warnings.push(format!(
                    "theme {name:?}: {key} = {value:?} is not a colour (use \"#rrggbb\" or a name like \"cyan\")"
                )),
            }
        }
        (theme, warnings)
    }

    fn slot_mut(&mut self, key: &str) -> Option<&mut Color> {
        Some(match key {
            "background" => &mut self.background,
            "text" => &mut self.text,
            "muted" => &mut self.muted,
            "subtle" => &mut self.subtle,
            "user" => &mut self.user,
            "assistant" => &mut self.assistant,
            "system" => &mut self.system,
            "tool" => &mut self.tool,
            "accent" => &mut self.accent,
            "on_accent" => &mut self.on_accent,
            "success" => &mut self.success,
            "warning" => &mut self.warning,
            "danger" => &mut self.danger,
            "info" => &mut self.info,
            "diff_add" => &mut self.diff_add,
            "diff_remove" => &mut self.diff_remove,
            "diff_hunk" => &mut self.diff_hunk,
            "heading1" => &mut self.heading1,
            "heading1_bg" => &mut self.heading1_bg,
            "heading" => &mut self.heading,
            "minor_heading" => &mut self.minor_heading,
            "code" => &mut self.code,
            "code_bg" => &mut self.code_bg,
            "link" => &mut self.link,
            "quote" => &mut self.quote,
            _ => return None,
        })
    }

    /// Base style for a region: theme text on theme background.
    pub fn base(&self) -> Style {
        Style::default().fg(self.text).bg(self.background)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::catppuccin_mocha()
    }
}

/// Built-in names followed by config-defined ones (sorted), without duplicates.
pub fn available_themes(custom: &HashMap<String, HashMap<String, String>>) -> Vec<String> {
    let mut extra: Vec<String> = custom
        .keys()
        .filter(|k| !BUILTIN_THEMES.contains(&k.as_str()))
        .cloned()
        .collect();
    extra.sort();
    BUILTIN_THEMES
        .iter()
        .map(|s| s.to_string())
        .chain(extra)
        .collect()
}

/// `"#rrggbb"`, a ratatui colour name (`"cyan"`, `"light-blue"`, `"dark-gray"`),
/// an ANSI index (`"208"`), or `"reset"`/`"none"` for the terminal default.
fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("none") {
        return Some(Color::Reset);
    }
    Color::from_str(s).ok()
}

impl tui_markdown::StyleSheet for Theme {
    fn heading(&self, level: u8) -> Style {
        match level {
            1 => Style::new()
                .fg(self.heading1)
                .bg(self.heading1_bg)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::new().fg(self.heading).add_modifier(Modifier::BOLD),
            3 => Style::new()
                .fg(self.heading)
                .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            _ => Style::new()
                .fg(self.minor_heading)
                .add_modifier(Modifier::ITALIC),
        }
    }

    fn code(&self) -> Style {
        Style::new().fg(self.code).bg(self.code_bg)
    }

    fn link(&self) -> Style {
        Style::new()
            .fg(self.link)
            .add_modifier(Modifier::UNDERLINED)
    }

    fn blockquote(&self) -> Style {
        Style::new().fg(self.quote)
    }

    fn heading_meta(&self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    fn metadata_block(&self) -> Style {
        Style::new().fg(self.warning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn builtins_resolve_without_warnings() {
        for name in BUILTIN_THEMES {
            let (theme, warnings) = Theme::resolve(name, &HashMap::new());
            assert_eq!(theme.name, *name);
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
        }
    }

    #[test]
    fn default_is_catppuccin_mocha() {
        assert_eq!(Theme::default().name, DEFAULT_THEME);
        assert_eq!(Theme::default().background, Color::Reset);
    }

    #[test]
    fn unknown_name_falls_back_with_warning() {
        let (theme, warnings) = Theme::resolve("nope", &HashMap::new());
        assert_eq!(theme.name, DEFAULT_THEME);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("classic"));
    }

    #[test]
    fn custom_theme_overrides_base() {
        let custom = HashMap::from([(
            "mine".to_string(),
            table(&[
                ("base", "classic"),
                ("accent", "#ff8800"),
                ("background", "none"),
            ]),
        )]);
        let (theme, warnings) = Theme::resolve("mine", &custom);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.name, "mine");
        assert_eq!(theme.accent, Color::Rgb(0xff, 0x88, 0x00));
        assert_eq!(theme.background, Color::Reset);
        assert_eq!(theme.user, Theme::classic().user);
    }

    #[test]
    fn custom_theme_defaults_to_mocha_base() {
        let custom = HashMap::from([("mine".to_string(), table(&[("user", "light-blue")]))]);
        let (theme, _) = Theme::resolve("mine", &custom);
        assert_eq!(theme.user, Color::LightBlue);
        assert_eq!(theme.accent, Theme::catppuccin_mocha().accent);
    }

    #[test]
    fn bad_keys_and_colours_are_reported_not_fatal() {
        let custom = HashMap::from([(
            "mine".to_string(),
            table(&[("acent", "red"), ("user", "not-a-colour"), ("tool", "red")]),
        )]);
        let (theme, warnings) = Theme::resolve("mine", &custom);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(theme.tool, Color::Red);
        assert_eq!(theme.user, Theme::catppuccin_mocha().user);
    }

    #[test]
    fn available_lists_builtins_first_then_custom_sorted() {
        let custom = HashMap::from([
            ("zed".to_string(), HashMap::new()),
            ("alpha".to_string(), HashMap::new()),
            ("classic".to_string(), HashMap::new()),
        ]);
        assert_eq!(
            available_themes(&custom),
            ["catppuccin-mocha", "classic", "alpha", "zed"]
        );
    }
}
