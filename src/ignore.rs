//! `.gitignore` matching.
//!
//! Rules follow gitignore(5): later patterns override earlier ones and deeper files override
//! shallower ones; `!` re-includes; a trailing `/` matches directories only; a pattern with a
//! slash in it is relative to its `.gitignore` directory, one without matches the name at any
//! depth; `*` and `?` do not cross a slash and `**` does. A file inside an ignored directory is
//! ignored whatever the patterns say.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    DoubleStar,
    Glob(Vec<char>),
}

#[derive(Debug, Clone)]
pub struct Pattern {
    segs: Vec<Seg>,
    negate: bool,
    dir_only: bool,
    anchored: bool,
}

/// Matches one path component against a glob with `*`, `?`, `[...]` and `\` escapes.
fn glob(p: &[char], t: &[char], icase: bool) -> bool {
    let eq = |a: char, b: char| if icase { a.to_lowercase().eq(b.to_lowercase()) } else { a == b };
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        let step = if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some((pi, ti));
                    pi += 1;
                    continue;
                }
                '?' => Some(1),
                '[' => class(&p[pi..], t[ti], icase),
                '\\' if pi + 1 < p.len() => eq(p[pi + 1], t[ti]).then_some(2),
                c => eq(c, t[ti]).then_some(1),
            }
        } else {
            None
        };
        match step {
            Some(n) => {
                pi += n;
                ti += 1;
            }
            None => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Matches a `[...]` class at the start of `p` against `c`; returns the pattern length consumed.
fn class(p: &[char], c: char, icase: bool) -> Option<usize> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let start = i;
    let mut matched = false;
    loop {
        let ch = *p.get(i)?;
        if ch == ']' && i > start {
            return (matched != negate).then_some(i + 1);
        }
        let lo = if ch == '\\' {
            i += 1;
            *p.get(i)?
        } else {
            ch
        };
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|e| *e != ']') {
            let hi = p[i + 2];
            let fold = |x: char| if icase { x.to_ascii_lowercase() } else { x };
            if (fold(lo)..=fold(hi)).contains(&fold(c)) {
                matched = true;
            }
            i += 3;
        } else {
            if lo == c || (icase && lo.to_lowercase().eq(c.to_lowercase())) {
                matched = true;
            }
            i += 1;
        }
    }
}

fn match_segs(segs: &[Seg], comps: &[&str], icase: bool) -> bool {
    match segs.split_first() {
        None => comps.is_empty(),
        Some((Seg::DoubleStar, rest)) => {
            if rest.is_empty() {
                // A trailing `**` matches everything inside, but not the directory itself.
                return !comps.is_empty();
            }
            (0..=comps.len()).any(|skip| match_segs(rest, &comps[skip..], icase))
        }
        Some((Seg::Glob(g), rest)) => match comps.split_first() {
            Some((c, tail)) => glob(g, &c.chars().collect::<Vec<_>>(), icase) && match_segs(rest, tail, icase),
            None => false,
        },
    }
}

impl Pattern {
    pub fn parse(line: &str) -> Option<Pattern> {
        let mut line = line.trim_end_matches('\r').to_string();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        // Trailing spaces are dropped unless escaped.
        while line.ends_with(' ') && !line.ends_with("\\ ") {
            line.pop();
        }
        if line.is_empty() {
            return None;
        }
        let negate = line.starts_with('!');
        // Drop the `!`, or the backslash that escapes a literal `!` or `#` at the start.
        if negate || line.starts_with("\\!") || line.starts_with("\\#") {
            line.remove(0);
        }
        let dir_only = line.ends_with('/');
        let body = line.trim_end_matches('/');
        if body.is_empty() {
            return None;
        }
        let anchored = body.contains('/');
        let body = body.strip_prefix('/').unwrap_or(body);
        let segs: Vec<Seg> =
            body.split('/').map(|s| if s == "**" { Seg::DoubleStar } else { Seg::Glob(s.chars().collect()) }).collect();
        Some(Pattern { segs, negate, dir_only, anchored })
    }

    /// Whether the pattern matches the path (components relative to the `.gitignore`'s directory).
    pub fn matches(&self, comps: &[&str], is_dir: bool, icase: bool) -> bool {
        if comps.is_empty() || (self.dir_only && !is_dir) {
            return false;
        }
        if self.anchored || self.segs.contains(&Seg::DoubleStar) {
            match_segs(&self.segs, comps, icase)
        } else {
            match &self.segs[0] {
                Seg::Glob(g) => glob(g, &comps[comps.len() - 1].chars().collect::<Vec<_>>(), icase),
                Seg::DoubleStar => false,
            }
        }
    }
}

/// The patterns of one ignore file and the directory (relative to the work tree) they apply to.
#[derive(Debug, Clone)]
pub struct IgnoreFile {
    pub dir: String,
    pub patterns: Vec<Pattern>,
}

impl IgnoreFile {
    pub fn parse(dir: &str, text: &str) -> IgnoreFile {
        IgnoreFile { dir: dir.to_string(), patterns: text.lines().filter_map(Pattern::parse).collect() }
    }
}

/// Decides whether paths are ignored, given the ignore files that apply (shallowest first).
#[derive(Debug, Clone, Default)]
pub struct Matcher {
    pub files: Vec<IgnoreFile>,
    pub ignore_case: bool,
}

impl Matcher {
    /// `Some(true)` ignored, `Some(false)` explicitly re-included, `None` no pattern matched.
    fn decide(&self, path: &str, is_dir: bool) -> Option<bool> {
        let mut result = None;
        for f in &self.files {
            let rel = if f.dir.is_empty() {
                path
            } else if let Some(r) = path.strip_prefix(&f.dir).and_then(|r| r.strip_prefix('/')) {
                r
            } else {
                continue;
            };
            let comps: Vec<&str> = rel.split('/').collect();
            for p in &f.patterns {
                if p.matches(&comps, is_dir, self.ignore_case) {
                    result = Some(!p.negate);
                }
            }
        }
        result
    }

    /// Whether a path is ignored. Every parent directory is checked first: nothing inside an
    /// ignored directory can be re-included.
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = path.split('/').collect();
        for i in 1..comps.len() {
            if self.decide(&comps[..i].join("/"), true) == Some(true) {
                return true;
            }
        }
        self.decide(path, is_dir) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(files: &[(&str, &str)]) -> Matcher {
        Matcher { files: files.iter().map(|(d, t)| IgnoreFile::parse(d, t)).collect(), ignore_case: false }
    }

    fn ig(m: &Matcher, path: &str) -> bool {
        m.is_ignored(path, false)
    }

    fn igd(m: &Matcher, path: &str) -> bool {
        m.is_ignored(path, true)
    }

    #[test]
    fn globs() {
        let g = |p: &str, t: &str| glob(&p.chars().collect::<Vec<_>>(), &t.chars().collect::<Vec<_>>(), false);
        assert!(g("*.rs", "main.rs") && !g("*.rs", "main.rc") && g("*", "") && g("a*b*c", "aXXbYYc"));
        assert!(g("?", "x") && !g("?", "") && !g("?", "xy"));
        assert!(g("[abc]x", "bx") && !g("[abc]x", "dx") && g("[a-c]x", "bx") && g("[!a-c]x", "dx") && !g("[!a-c]x", "bx"));
        assert!(g("[]a]", "]") && g("[a\\]]", "]"));
        assert!(g("\\*", "*") && !g("\\*", "x"));
        assert!(g("a*", "a") && g("*a", "a") && !g("*a", "ab") && g("**", "anything"));
        assert!(g("*a*a*a*b", "aaaaab") && !g("*a*a*a*b", "aaaaa"));
        // The pathological backtracking case terminates quickly.
        assert!(!g(&"a*".repeat(30), &"a".repeat(29)));
        let ic = |p: &str, t: &str| glob(&p.chars().collect::<Vec<_>>(), &t.chars().collect::<Vec<_>>(), true);
        assert!(ic("*.RS", "main.rs") && ic("[A-C]x", "bx"));
    }

    #[test]
    fn basic_patterns() {
        let m = matcher(&[("", "*.log\nbuild/\n/root-only\ntmp\n")]);
        assert!(ig(&m, "debug.log") && ig(&m, "deep/er/debug.log") && !ig(&m, "debug.txt"));
        assert!(igd(&m, "build") && igd(&m, "src/build") && !ig(&m, "build"), "a trailing slash matches directories only");
        assert!(ig(&m, "build/out.o") && ig(&m, "src/build/out.o"), "contents of an ignored directory");
        assert!(ig(&m, "root-only") && !ig(&m, "sub/root-only"), "a leading slash anchors");
        assert!(ig(&m, "tmp") && ig(&m, "a/tmp") && igd(&m, "a/tmp"));
    }

    #[test]
    fn negation_and_precedence() {
        let m = matcher(&[("", "*.log\n!keep.log\n")]);
        assert!(ig(&m, "a.log") && !ig(&m, "keep.log") && !ig(&m, "dir/keep.log"));
        // The order matters: the last matching pattern wins.
        let m = matcher(&[("", "!keep.log\n*.log\n")]);
        assert!(ig(&m, "keep.log"));
        // A deeper file overrides a shallower one.
        let m = matcher(&[("", "*.tmp\n"), ("sub", "!*.tmp\n")]);
        assert!(ig(&m, "a.tmp") && !ig(&m, "sub/a.tmp") && !ig(&m, "sub/deeper/a.tmp"));
        // A file in a directory that is ignored cannot be re-included.
        let m = matcher(&[("", "out/\n!out/keep.txt\n")]);
        assert!(ig(&m, "out/keep.txt"));
        // But `out/*` with a re-include works, because `out` itself is not ignored.
        let m = matcher(&[("", "out/*\n!out/keep.txt\n")]);
        assert!(ig(&m, "out/a.txt") && !ig(&m, "out/keep.txt"));
    }

    #[test]
    fn slashes_and_double_stars() {
        let m = matcher(&[("", "a/b\n/c/*.o\n**/gen\nx/**/y\nz/**\n")]);
        assert!(ig(&m, "a/b") && !ig(&m, "q/a/b"), "a pattern with a slash is anchored");
        assert!(ig(&m, "c/main.o") && !ig(&m, "c/d/main.o") && !ig(&m, "d/c/main.o"), "a star does not cross a slash");
        assert!(ig(&m, "gen") && ig(&m, "p/q/gen") && ig(&m, "gen/inner"));
        assert!(ig(&m, "x/y") && ig(&m, "x/1/y") && ig(&m, "x/1/2/y") && !ig(&m, "x/1/2/yy"));
        assert!(ig(&m, "z/anything") && ig(&m, "z/a/b") && !ig(&m, "z"), "trailing ** matches the inside only");
    }

    #[test]
    fn nested_ignore_files_are_relative_to_their_directory() {
        let m = matcher(&[("", "root.txt\n"), ("sub", "/only-here\nlocal/*.x\n")]);
        assert!(ig(&m, "sub/only-here") && !ig(&m, "only-here") && !ig(&m, "sub/deeper/only-here"));
        assert!(ig(&m, "sub/local/a.x") && !ig(&m, "local/a.x") && !ig(&m, "other/local/a.x"));
        assert!(ig(&m, "sub/root.txt"), "the root file still applies below");
    }

    #[test]
    fn comments_blank_lines_escapes_and_spaces() {
        let m = matcher(&[("", "# comment\n\n\\#hash\n\\!bang\ntrailing   \nescaped\\ \n  leading\n")]);
        assert!(ig(&m, "#hash") && ig(&m, "!bang") && ig(&m, "trailing") && ig(&m, "escaped "));
        assert!(ig(&m, "  leading") && !ig(&m, "leading"));
        assert!(!ig(&m, "comment") && !ig(&m, "# comment"));
    }

    #[test]
    fn case_insensitive_mode() {
        let mut m = matcher(&[("", "*.LOG\nBuild/\n")]);
        assert!(!ig(&m, "a.log"));
        m.ignore_case = true;
        assert!(ig(&m, "a.log") && igd(&m, "build"));
    }

    #[test]
    fn crlf_files() {
        let m = matcher(&[("", "a.txt\r\nb.txt\r\n")]);
        assert!(ig(&m, "a.txt") && ig(&m, "b.txt"));
    }
}
