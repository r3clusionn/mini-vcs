//! Reading and editing a git config file (`[section]` / `[section "sub"]` headers and
//! `key = value` lines). Only what a repository needs: keys are case-insensitive, values may be
//! quoted, `#` and `;` start comments, and editing keeps every other line as it was.

use crate::err;
use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct Config {
    lines: Vec<String>,
}

/// `[core]`, `[branch "main"]` -> `core`, `branch.main`.
fn parse_header(line: &str) -> Option<String> {
    let inner = line.trim().strip_prefix('[')?;
    let end = inner.find(']')?;
    let inner = &inner[..end];
    match inner.split_once(' ') {
        Some((sec, sub)) => {
            let sub = sub.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some(format!("{}.{}", sec.to_ascii_lowercase(), sub))
        }
        None => Some(inner.trim().to_ascii_lowercase()),
    }
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    let mut out = String::new();
    // Characters that were quoted or escaped are never trimmed, only unquoted trailing spaces.
    let mut protected = 0;
    let mut in_quotes = false;
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' => {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(o) => out.push(o),
                    None => {}
                }
                protected = out.len();
            }
            '#' | ';' if !in_quotes => break,
            _ => {
                out.push(c);
                if in_quotes {
                    protected = out.len();
                }
            }
        }
    }
    let trimmed = out.trim_end().len().max(protected);
    out.truncate(trimmed);
    out
}

fn parse_key_line(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with(';') || t.starts_with('[') {
        return None;
    }
    match t.split_once('=') {
        Some((k, v)) => Some((k.trim().to_ascii_lowercase(), unquote(v))),
        // A bare key means true.
        None => Some((t.to_ascii_lowercase(), "true".to_string())),
    }
}

impl Config {
    pub fn parse(text: &str) -> Config {
        Config { lines: text.lines().map(str::to_string).collect() }
    }

    pub fn to_text(&self) -> String {
        let mut s = self.lines.join("\n");
        if !s.is_empty() {
            s.push('\n');
        }
        s
    }

    /// The last value of `section.key` or `section.sub.key`.
    pub fn get(&self, name: &str) -> Option<String> {
        let (section, key) = split_name(name).ok()?;
        let mut current = String::new();
        let mut value = None;
        for line in &self.lines {
            if let Some(h) = parse_header(line) {
                current = h;
            } else if current == section {
                if let Some((k, v)) = parse_key_line(line) {
                    if k == key {
                        value = Some(v);
                    }
                }
            }
        }
        value
    }

    pub fn get_bool(&self, name: &str) -> Option<bool> {
        match self.get(name)?.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        }
    }

    /// Sets `section.key`, replacing the last existing value or adding the key (and the section).
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        let (section, key) = split_name(name)?;
        if value.contains('\n') {
            return Err(err!("a config value cannot contain a newline"));
        }
        let quoted =
            if value.is_empty() || value.starts_with(' ') || value.ends_with(' ') || value.contains(['#', ';', '"', '\\']) {
                format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                value.to_string()
            };
        let line = format!("\t{key} = {quoted}");
        let mut current = String::new();
        let mut last_in_section: Option<usize> = None;
        let mut existing: Option<usize> = None;
        for (i, l) in self.lines.iter().enumerate() {
            if let Some(h) = parse_header(l) {
                current = h;
                if current == section {
                    last_in_section = Some(i);
                }
            } else if current == section {
                last_in_section = Some(i);
                if parse_key_line(l).is_some_and(|(k, _)| k == key) {
                    existing = Some(i);
                }
            }
        }
        match (existing, last_in_section) {
            (Some(i), _) => self.lines[i] = line,
            (None, Some(i)) => self.lines.insert(i + 1, line),
            (None, None) => {
                let header = match section.split_once('.') {
                    Some((sec, sub)) => format!("[{sec} \"{sub}\"]"),
                    None => format!("[{section}]"),
                };
                self.lines.push(header);
                self.lines.push(line);
            }
        }
        Ok(())
    }
}

/// `user.name` -> (`user`, `name`); `branch.main.remote` -> (`branch.main`, `remote`).
fn split_name(name: &str) -> Result<(String, String)> {
    let i = name.rfind('.').ok_or_else(|| err!("invalid config key {name:?} (expected section.key)"))?;
    let (section, key) = (&name[..i], &name[i + 1..]);
    if section.is_empty() || key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(err!("invalid config key {name:?}"));
    }
    let section = match section.split_once('.') {
        Some((s, sub)) => format!("{}.{}", s.to_ascii_lowercase(), sub),
        None => section.to_ascii_lowercase(),
    };
    Ok((section, key.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tBare = false\n[user]\n\tname = Jane Q. Public\n\temail = \"jane@example.com\" # work\n[branch \"main\"]\n\tremote = origin\n";

    #[test]
    fn reading() {
        let c = Config::parse(SAMPLE);
        assert_eq!(c.get("core.filemode").as_deref(), Some("false"));
        assert_eq!(c.get("CORE.BARE").as_deref(), Some("false"));
        assert_eq!(c.get("user.name").as_deref(), Some("Jane Q. Public"));
        assert_eq!(c.get("user.email").as_deref(), Some("jane@example.com"));
        assert_eq!(c.get("branch.main.remote").as_deref(), Some("origin"));
        assert_eq!(c.get("branch.other.remote"), None);
        assert_eq!(c.get("user.missing"), None);
        assert_eq!(c.get_bool("core.filemode"), Some(false));
        assert_eq!(c.get_bool("user.name"), None);
        assert_eq!(c.get("nodot"), None);
    }

    #[test]
    fn last_value_wins_and_bare_keys_are_true() {
        let c = Config::parse("[a]\n\tx = 1\n\tflag\n\tx = 2\n");
        assert_eq!(c.get("a.x").as_deref(), Some("2"));
        assert_eq!(c.get_bool("a.flag"), Some(true));
    }

    #[test]
    fn editing_keeps_everything_else() {
        let mut c = Config::parse(SAMPLE);
        c.set("user.name", "New Name").unwrap();
        c.set("user.signingkey", "ABC").unwrap();
        c.set("core.ignorecase", "true").unwrap();
        c.set("remote.origin.url", "https://example.com/x.git").unwrap();
        let t = c.to_text();
        assert!(t.contains("\tname = New Name\n") && !t.contains("Jane"));
        assert!(t.contains("\temail = \"jane@example.com\" # work\n"), "untouched lines stay as they were");
        assert_eq!(c.get("user.signingkey").as_deref(), Some("ABC"));
        assert_eq!(c.get("core.ignorecase").as_deref(), Some("true"));
        assert!(t.contains("[remote \"origin\"]\n\turl = https://example.com/x.git\n"));
        // New keys go inside their own section.
        let user_at = t.find("[user]").unwrap();
        let branch_at = t.find("[branch").unwrap();
        assert!(t.find("signingkey").unwrap() > user_at && t.find("signingkey").unwrap() < branch_at);
        assert_eq!(Config::parse(&t).to_text(), t);
    }

    #[test]
    fn values_needing_quotes_round_trip() {
        let mut c = Config::new_for_test();
        for v in [
            "plain",
            "with space inside",
            " leading",
            "trailing ",
            "has # hash",
            "semi;colon",
            "quote\"inside",
            "back\\slash",
            "",
        ] {
            c.set("x.y", v).unwrap();
            assert_eq!(Config::parse(&c.to_text()).get("x.y").as_deref(), Some(v), "{v:?}");
        }
        assert!(c.set("x.y", "two\nlines").is_err());
        assert!(c.set("nodot", "v").is_err());
        assert!(c.set("a.", "v").is_err());
        assert!(c.set(".k", "v").is_err());
        assert!(c.set("a.b c", "v").is_err());
    }

    impl Config {
        fn new_for_test() -> Config {
            Config::default()
        }
    }

    #[test]
    fn empty_config() {
        let c = Config::parse("");
        assert_eq!(c.get("a.b"), None);
        assert_eq!(c.to_text(), "");
    }
}
