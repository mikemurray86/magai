use regex::RegexBuilder;
use rig::completion::request::ToolDefinition;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use walkdir::WalkDir;

fn default_max_results() -> usize {
    50
}

#[derive(Deserialize)]
pub struct GrepSearchArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

#[derive(Deserialize, Serialize)]
pub struct GrepSearch;

impl Tool for GrepSearch {
    const NAME: &'static str = "grep_search";
    type Error = std::io::Error;
    type Args = GrepSearchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "grep_search".to_string(),
            description: "Search file contents by regex pattern. Returns matching file:line:text results. Prefer this over shell_command for content searches.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "regex pattern to search for" },
                    "path": { "type": "string", "description": "file or directory to search (defaults to current directory)" },
                    "glob": { "type": "string", "description": "filter files by glob pattern, e.g. '*.rs' or '**/*.toml'" },
                    "case_sensitive": { "type": "boolean", "description": "case-sensitive match (default false)" },
                    "max_results": { "type": "integer", "description": "maximum number of matches to return (default 50)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let re = RegexBuilder::new(&args.pattern)
            .case_insensitive(!args.case_sensitive)
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;

        let root = match args.path {
            Some(ref p) => PathBuf::from(p),
            None => std::env::current_dir()?,
        };

        let glob_pat = args.glob.as_deref();
        let mut matches = Vec::new();
        let mut truncated = false;

        // If root is a single file, search only that file.
        if root.is_file() {
            search_file(&root, &re, &mut matches, args.max_results, &mut truncated);
        } else {
            'outer: for entry in WalkDir::new(&root)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                if let Some(pat) = glob_pat {
                    let name = entry.file_name().to_string_lossy();
                    if !glob_match(pat, &name) {
                        // also try matching against the relative path
                        let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                        if !glob_match(pat, &rel.to_string_lossy()) {
                            continue;
                        }
                    }
                }
                // Skip binary-looking files
                if is_likely_binary(entry.path()) {
                    continue;
                }
                search_file(
                    entry.path(),
                    &re,
                    &mut matches,
                    args.max_results,
                    &mut truncated,
                );
                if truncated {
                    break 'outer;
                }
            }
        }

        Ok(json!({ "matches": matches, "truncated": truncated }).to_string())
    }
}

fn search_file(
    path: &std::path::Path,
    re: &regex::Regex,
    matches: &mut Vec<serde_json::Value>,
    max_results: usize,
    truncated: &mut bool,
) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = BufReader::new(file);
    for (idx, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return,
        };
        if re.is_match(&line) {
            matches.push(json!({
                "file": path.to_string_lossy(),
                "line": idx + 1,
                "text": line
            }));
            if matches.len() >= max_results {
                *truncated = true;
                return;
            }
        }
    }
}

fn glob_match(pattern: &str, name: &str) -> bool {
    // Simple glob: support * (any chars except /) and ** (any chars including /)
    let re_str = glob_to_regex(pattern);
    regex::Regex::new(&re_str)
        .map(|r| r.is_match(name))
        .unwrap_or(false)
}

fn glob_to_regex(pattern: &str) -> String {
    let mut result = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                result.push_str(".*");
            }
            '*' => result.push_str("[^/]*"),
            '?' => result.push_str("[^/]"),
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                result.push('\\');
                result.push(c);
            }
            _ => result.push(c),
        }
    }
    result.push('$');
    result
}

fn is_likely_binary(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "bmp"
                | "ico"
                | "webp"
                | "svg"
                | "pdf"
                | "zip"
                | "tar"
                | "gz"
                | "bz2"
                | "xz"
                | "7z"
                | "rar"
                | "exe"
                | "dll"
                | "so"
                | "dylib"
                | "a"
                | "o"
                | "obj"
                | "wasm"
                | "class"
                | "jar"
                | "pyc"
                | "mp3"
                | "mp4"
                | "wav"
                | "ogg"
                | "flac"
                | "avi"
                | "mov"
                | "ttf"
                | "otf"
                | "woff"
                | "woff2"
        )
    )
}
