//! Turning changes into text: file diffs in git's format, commit headers and dates.

use std::collections::BTreeMap;

use crate::diff::{diff as diff_edits, hunks, is_binary, split_lines, Edit};
use crate::err;
use crate::error::Result;
use crate::object::{Commit, Kind, Signature};
use crate::repo::Repo;
use crate::sha1::Oid;

/// One side of a file change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Side {
    pub mode: u32,
    pub oid: Oid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub old: Option<Side>,
    pub new: Option<Side>,
}

impl Change {
    pub fn status_letter(&self) -> char {
        match (&self.old, &self.new) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            _ => 'M',
        }
    }
}

type Files = BTreeMap<String, (u32, Oid)>;

/// The differences between two file maps, in path order.
pub fn changes(old: &Files, new: &Files) -> Vec<Change> {
    let mut paths: Vec<&String> = old.keys().chain(new.keys()).collect();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter(|p| old.get(*p) != new.get(*p))
        .map(|p| Change {
            path: p.clone(),
            old: old.get(p).map(|(m, o)| Side { mode: *m, oid: *o }),
            new: new.get(p).map(|(m, o)| Side { mode: *m, oid: *o }),
        })
        .collect()
}

/// Renders the unified diff of changes the way `git diff` does (no rename detection).
/// `read` supplies the content of an object id.
pub fn render_changes(list: &[Change], read: &dyn Fn(&Oid) -> Result<Vec<u8>>) -> Result<String> {
    let mut out = String::new();
    for c in list {
        let (a, b) = (format!("a/{}", c.path), format!("b/{}", c.path));
        out.push_str(&format!("diff --git {a} {b}\n"));
        let empty = crate::object::object_id(Kind::Blob, b"");
        let old_data = match &c.old {
            Some(s) => read(&s.oid)?,
            None => Vec::new(),
        };
        let new_data = match &c.new {
            Some(s) => read(&s.oid)?,
            None => Vec::new(),
        };
        let zero = "0000000";
        match (&c.old, &c.new) {
            (None, Some(n)) => out.push_str(&format!("new file mode {:o}\nindex {zero}..{}\n", n.mode, &n.oid.hex()[..7])),
            (Some(o), None) => out.push_str(&format!("deleted file mode {:o}\nindex {}..{zero}\n", o.mode, &o.oid.hex()[..7])),
            (Some(o), Some(n)) => {
                if o.mode != n.mode {
                    out.push_str(&format!("old mode {:o}\nnew mode {:o}\n", o.mode, n.mode));
                }
                if o.oid != n.oid {
                    let mode = if o.mode == n.mode { format!(" {:o}", n.mode) } else { String::new() };
                    out.push_str(&format!("index {}..{}{mode}\n", &o.oid.hex()[..7], &n.oid.hex()[..7]));
                }
            }
            (None, None) => {}
        }
        let same_content = c.old.as_ref().map(|s| s.oid) == c.new.as_ref().map(|s| s.oid);
        if same_content {
            continue;
        }
        // An empty file added or removed has no ---/+++ lines.
        if (c.old.is_none() && c.new.as_ref().is_some_and(|n| n.oid == empty))
            || (c.new.is_none() && c.old.as_ref().is_some_and(|o| o.oid == empty))
        {
            continue;
        }
        let (old_name, new_name) =
            (if c.old.is_some() { a.as_str() } else { "/dev/null" }, if c.new.is_some() { b.as_str() } else { "/dev/null" });
        if is_binary(&old_data) || is_binary(&new_data) {
            out.push_str(&format!("Binary files {old_name} and {new_name} differ\n"));
            continue;
        }
        out.push_str(&format!("--- {old_name}\n+++ {new_name}\n"));
        let (ot, nt) = (String::from_utf8_lossy(&old_data), String::from_utf8_lossy(&new_data));
        out.push_str(&hunk_text(&ot, &nt));
    }
    Ok(out)
}

fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{}", start + 1, len),
    }
}

/// The text git puts after the second `@@`: the last line before the hunk that starts with a
/// letter, `_` or `$` (git's default "function" pattern), cut to 80 bytes.
fn hunk_context(old_lines: &[&str], hunk_start: usize) -> String {
    for line in old_lines[..hunk_start.min(old_lines.len())].iter().rev() {
        if line.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$') {
            let text = line.trim_end();
            let mut end = text.len().min(80);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            return format!(" {}", &text[..end]);
        }
    }
    String::new()
}

fn hunk_text(old: &str, new: &str) -> String {
    let (a, b) = (split_lines(old), split_lines(new));
    let edits: Vec<Edit> = diff_edits(&a, &b);
    let mut out = String::new();
    for h in hunks(&edits, &a, &b, 3) {
        out.push_str(&format!(
            "@@ -{} +{} @@{}\n",
            range(h.a_start, h.a_len),
            range(h.b_start, h.b_len),
            hunk_context(&a, h.a_start)
        ));
        for (c, line) in &h.lines {
            out.push(*c);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `Thu Jan 1 00:00:00 1970 +0000`, in the signature's own time zone.
pub fn format_date(when: i64, tz: &str) -> String {
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let offset = sign
        * (tz.get(1..3).and_then(|h| h.parse::<i64>().ok()).unwrap_or(0) * 3600
            + tz.get(3..5).and_then(|m| m.parse::<i64>().ok()).unwrap_or(0) * 60);
    let local = when + offset;
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{} {} {} {:02}:{:02}:{:02} {} {}",
        WEEKDAYS[days.rem_euclid(7) as usize],
        MONTHS[(m - 1) as usize],
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        y,
        tz
    )
}

/// A commit as `git log` prints it.
pub fn format_commit(id: &Oid, c: &Commit) -> String {
    let mut s = format!("commit {id}\n");
    if c.parents.len() > 1 {
        let ps: Vec<String> = c.parents.iter().map(|p| p.short()).collect();
        s.push_str(&format!("Merge: {}\n", ps.join(" ")));
    }
    s.push_str(&format!(
        "Author: {} <{}>\nDate:   {}\n\n",
        c.author.name,
        c.author.email,
        format_date(c.author.when, &c.author.tz)
    ));
    for line in c.message.trim_end_matches('\n').split('\n') {
        // Every line is indented, blank ones too.
        s.push_str(&format!("    {line}\n"));
    }
    s
}

/// A short custom format: `%H %h %T %P %p %an %ae %ad %at %cn %ce %ct %s %b %n %%`.
pub fn format_custom(fmt: &str, id: &Oid, c: &Commit) -> String {
    let sig = |s: &Signature, what: char| match what {
        'n' => s.name.clone(),
        'e' => s.email.clone(),
        'd' => format_date(s.when, &s.tz),
        't' => s.when.to_string(),
        _ => String::new(),
    };
    let mut out = String::new();
    let mut chars = fmt.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('H') => out.push_str(&id.hex()),
            Some('h') => out.push_str(&id.short()),
            Some('T') => out.push_str(&c.tree.hex()),
            Some('P') => out.push_str(&c.parents.iter().map(|p| p.hex()).collect::<Vec<_>>().join(" ")),
            Some('p') => out.push_str(&c.parents.iter().map(|p| p.short()).collect::<Vec<_>>().join(" ")),
            Some('a') => out.push_str(&sig(&c.author, chars.next().unwrap_or(' '))),
            Some('c') => out.push_str(&sig(&c.committer, chars.next().unwrap_or(' '))),
            Some('s') => out.push_str(c.subject()),
            Some('b') => out.push_str(c.message.split_once("\n\n").map(|(_, b)| b).unwrap_or("").trim_end_matches('\n')),
            Some('n') => out.push('\n'),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

impl Repo {
    pub fn render_diff(&self, list: &[Change]) -> Result<String> {
        render_changes(list, &|oid| {
            if let Some(data) = self.scratch.lock().unwrap_or_else(|e| e.into_inner()).get(oid) {
                return Ok(data.clone());
            }
            self.odb.read_kind(oid, Kind::Blob)
        })
    }

    /// Changes from `HEAD` to the index (`git diff --cached`).
    pub fn diff_cached(&self) -> Result<Vec<Change>> {
        let index = self.read_index()?;
        let head = self.commit_files(self.head_commit()?.as_ref())?;
        let staged: Files = index.entries().filter(|e| e.stage == 0).map(|e| (e.path.clone(), (e.mode, e.oid))).collect();
        Ok(changes(&head, &staged))
    }

    /// Changes from the index to the work tree (`git diff`). Nothing is written to the object
    /// store; the content of changed files is kept in memory for [`Repo::render_diff`].
    pub fn diff_work(&self) -> Result<Vec<Change>> {
        use crate::worktree::WorkState;
        let index = self.read_index()?;
        let mut out = Vec::new();
        for e in index.entries().filter(|e| e.stage == 0) {
            // Files whose stat data still matches the index cannot have changed.
            if self.work_matches(e, index.written)? == WorkState::Same {
                continue;
            }
            let abs = self.work.join(&e.path);
            let new = match std::fs::symlink_metadata(&abs) {
                Ok(m) if !m.is_dir() => {
                    let data = if m.file_type().is_symlink() {
                        std::fs::read_link(&abs)?.to_string_lossy().replace('\\', "/").into_bytes()
                    } else {
                        std::fs::read(&abs).map_err(|er| err!("{}: {er}", e.path))?
                    };
                    let oid = crate::object::object_id(Kind::Blob, &data);
                    self.scratch.lock().unwrap_or_else(|er| er.into_inner()).insert(oid, data);
                    Some(Side { mode: e.mode, oid })
                }
                _ => None,
            };
            let old = Some(Side { mode: e.mode, oid: e.oid });
            if old != new {
                out.push(Change { path: e.path.clone(), old, new });
            }
        }
        Ok(out)
    }

    pub fn diff_commits(&self, a: Option<&Oid>, b: Option<&Oid>) -> Result<Vec<Change>> {
        Ok(changes(&self.commit_files(a)?, &self.commit_files(b)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{object_id, MODE_EXEC, MODE_FILE};
    use std::collections::HashMap;

    struct Store(HashMap<Oid, Vec<u8>>);

    impl Store {
        fn put(&mut self, data: &[u8]) -> Oid {
            let id = object_id(Kind::Blob, data);
            self.0.insert(id, data.to_vec());
            id
        }
    }

    fn render(store: &Store, list: &[Change]) -> String {
        render_changes(list, &|oid| Ok(store.0.get(oid).cloned().unwrap())).unwrap()
    }

    #[test]
    fn dates_match_gits_format() {
        assert_eq!(format_date(0, "+0000"), "Thu Jan 1 00:00:00 1970 +0000");
        assert_eq!(format_date(1_700_000_000, "+0000"), "Tue Nov 14 22:13:20 2023 +0000");
        assert_eq!(format_date(1_700_000_000, "+0530"), "Wed Nov 15 03:43:20 2023 +0530");
        assert_eq!(format_date(1_700_000_000, "-0800"), "Tue Nov 14 14:13:20 2023 -0800");
        assert_eq!(format_date(951_782_400, "+0000"), "Tue Feb 29 00:00:00 2000 +0000");
    }

    #[test]
    fn modified_added_deleted_and_mode_changes() {
        let mut st = Store(HashMap::new());
        let a = st.put(b"one\ntwo\n");
        let b = st.put(b"one\n2\n");
        let empty = st.put(b"");
        let list = vec![
            Change { path: "added.txt".into(), old: None, new: Some(Side { mode: MODE_FILE, oid: a }) },
            Change { path: "empty-new".into(), old: None, new: Some(Side { mode: MODE_FILE, oid: empty }) },
            Change { path: "gone.txt".into(), old: Some(Side { mode: MODE_FILE, oid: a }), new: None },
            Change {
                path: "mod.txt".into(),
                old: Some(Side { mode: MODE_FILE, oid: a }),
                new: Some(Side { mode: MODE_FILE, oid: b }),
            },
            Change {
                path: "chmod.sh".into(),
                old: Some(Side { mode: MODE_FILE, oid: a }),
                new: Some(Side { mode: MODE_EXEC, oid: a }),
            },
            Change {
                path: "both.sh".into(),
                old: Some(Side { mode: MODE_FILE, oid: a }),
                new: Some(Side { mode: MODE_EXEC, oid: b }),
            },
        ];
        let text = render(&st, &list);
        let (a7, b7) = (&a.hex()[..7], &b.hex()[..7]);
        let e7 = &empty.hex()[..7];
        assert!(text.contains(&format!("diff --git a/added.txt b/added.txt\nnew file mode 100644\nindex 0000000..{a7}\n--- /dev/null\n+++ b/added.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n")));
        assert!(
            text.contains(&format!("diff --git a/empty-new b/empty-new\nnew file mode 100644\nindex 0000000..{e7}\ndiff --git"))
        );
        assert!(text.contains(&format!(
            "deleted file mode 100644\nindex {a7}..0000000\n--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n"
        )));
        assert!(
            text.contains(&format!("index {a7}..{b7} 100644\n--- a/mod.txt\n+++ b/mod.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+2\n"))
        );
        assert!(text.contains("diff --git a/chmod.sh b/chmod.sh\nold mode 100644\nnew mode 100755\ndiff --git"));
        assert!(text.contains(&format!(
            "diff --git a/both.sh b/both.sh\nold mode 100644\nnew mode 100755\nindex {a7}..{b7}\n--- a/both.sh"
        )));
    }

    #[test]
    fn binary_files() {
        let mut st = Store(HashMap::new());
        let bin = st.put(b"abc\0def");
        let txt = st.put(b"text\n");
        let text = render(
            &st,
            &[Change {
                path: "x.bin".into(),
                old: Some(Side { mode: MODE_FILE, oid: txt }),
                new: Some(Side { mode: MODE_FILE, oid: bin }),
            }],
        );
        assert!(text.ends_with("Binary files a/x.bin and b/x.bin differ\n"), "{text}");
        let text = render(&st, &[Change { path: "n.bin".into(), old: None, new: Some(Side { mode: MODE_FILE, oid: bin }) }]);
        assert!(text.ends_with("Binary files /dev/null and b/n.bin differ\n"), "{text}");
    }

    #[test]
    fn change_detection() {
        let id = |n: u8| crate::sha1::sha1(&[n]);
        let old: Files =
            [("a".to_string(), (MODE_FILE, id(1))), ("b".to_string(), (MODE_FILE, id(2))), ("c".to_string(), (MODE_FILE, id(3)))]
                .into();
        let new: Files =
            [("a".to_string(), (MODE_FILE, id(1))), ("b".to_string(), (MODE_FILE, id(9))), ("d".to_string(), (MODE_FILE, id(4)))]
                .into();
        let ch = changes(&old, &new);
        let letters: Vec<(String, char)> = ch.iter().map(|c| (c.path.clone(), c.status_letter())).collect();
        assert_eq!(letters, vec![("b".into(), 'M'), ("c".into(), 'D'), ("d".into(), 'A')]);
        assert!(changes(&old, &old).is_empty());
    }

    #[test]
    fn commit_formats() {
        let sig = Signature { name: "A".into(), email: "a@b".into(), when: 1_700_000_000, tz: "+0000".into() };
        let c = Commit {
            tree: crate::sha1::sha1(b"t"),
            parents: vec![crate::sha1::sha1(b"p1"), crate::sha1::sha1(b"p2")],
            author: sig.clone(),
            committer: sig,
            extra: vec![],
            message: "Subject\n\nBody line\n".into(),
        };
        let id = crate::sha1::sha1(b"c");
        let text = format_commit(&id, &c);
        assert!(text.starts_with(&format!("commit {id}\nMerge: ")));
        assert!(text.ends_with("Date:   Tue Nov 14 22:13:20 2023 +0000\n\n    Subject\n    \n    Body line\n"), "{text}");
        assert_eq!(
            format_custom("%h|%s|%an|%ae|%at|%%|%b", &id, &c),
            format!("{}|Subject|A|a@b|1700000000|%|Body line", id.short())
        );
    }
}
