//! Minimal glob matching shared by `find_files` and `grep_search`.
//!
//! Supports `*` (any chars except `/`), `**` (any chars including `/`), and
//! `?` (single char except `/`); everything else is matched literally.

pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let re_str = glob_to_regex(pattern);
    regex::Regex::new(&re_str)
        .map(|r| r.is_match(name))
        .unwrap_or(false)
}

pub(crate) fn glob_to_regex(pattern: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_literal() {
        assert!(glob_match("foo.rs", "foo.rs"));
        assert!(!glob_match("foo.rs", "bar.rs"));
    }

    #[test]
    fn matches_single_star_excludes_slash() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
    }

    #[test]
    fn matches_double_star_includes_slash() {
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "a/b/main.rs"));
        assert!(!glob_match("**/*.rs", "main.rs"));
        assert!(glob_match("src/**", "src/a/b/c.rs"));
    }

    #[test]
    fn matches_question_mark_single_char() {
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
        assert!(!glob_match("?.rs", "/.rs"));
    }

    #[test]
    fn no_match_cases() {
        assert!(!glob_match("*.toml", "Cargo.lock"));
        assert!(!glob_match("test_*", "other_test"));
    }
}
