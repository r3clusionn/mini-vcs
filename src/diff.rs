//! Line diffs (Myers' O(ND) algorithm), unified diff output and a three-way line merge.

use std::fmt::Write;

/// Splits text into lines, each keeping its line ending, so a last line without a newline
/// differs from the same line with one.
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Equal,
    Delete,
    Insert,
}

/// One step of an edit script: the operation and the 0-based line numbers it concerns in `a`
/// and `b` (for a delete only `a` advances, for an insert only `b`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub op: Op,
    pub a: usize,
    pub b: usize,
}

/// A shortest edit script turning `a` into `b`.
pub fn diff<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Edit> {
    // Common prefix and suffix are equal in every shortest script and cost nothing to skip.
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    let mut edits: Vec<Edit> = (0..prefix).map(|i| Edit { op: Op::Equal, a: i, b: i }).collect();
    for e in myers(a_mid, b_mid) {
        edits.push(Edit { op: e.op, a: e.a + prefix, b: e.b + prefix });
    }
    for i in 0..suffix {
        edits.push(Edit { op: Op::Equal, a: a.len() - suffix + i, b: b.len() - suffix + i });
    }
    edits
}

fn myers<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Edit> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 {
        return (0..b.len()).map(|j| Edit { op: Op::Insert, a: 0, b: j }).collect();
    }
    if m == 0 {
        return (0..a.len()).map(|i| Edit { op: Op::Delete, a: i, b: 0 }).collect();
    }
    let max = n + m;
    let offset = max;
    let mut v = vec![0isize; (2 * max + 2) as usize];
    // trace[d] is the furthest-reaching x of each diagonal after d edits (only the used range).
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'search: for d in 0..=max {
        trace.push(v[(offset - d).max(0) as usize..=(offset + d + 1).min(2 * max + 1) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let idx = (offset + k) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) { v[idx + 1] } else { v[idx - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'search;
            }
            k += 2;
        }
    }
    let d_final = found.expect("a script always exists");

    // Walk back from (n, m) to (0, 0) using the saved frontiers.
    let mut edits = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..=d_final).rev() {
        let lo = (offset - d).max(0);
        let at = |k: isize, vv: &[isize]| -> isize { vv[(offset + k - lo) as usize] };
        // `trace[d]` holds the frontier from the previous round (d - 1), which is what the step at d chose from.
        let prev = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1, prev) < at(k + 1, prev)) { k + 1 } else { k - 1 };
        let prev_x = if d == 0 { 0 } else { at(prev_k, prev) };
        let prev_y = prev_x - prev_k;
        // Diagonal moves (equal lines) back to the end of the edit.
        while x > prev_x && y > prev_y && d > 0 {
            x -= 1;
            y -= 1;
            edits.push(Edit { op: Op::Equal, a: x as usize, b: y as usize });
        }
        if d == 0 {
            while x > 0 && y > 0 {
                x -= 1;
                y -= 1;
                edits.push(Edit { op: Op::Equal, a: x as usize, b: y as usize });
            }
            break;
        }
        if x == prev_x {
            y -= 1;
            edits.push(Edit { op: Op::Insert, a: x as usize, b: y as usize });
        } else {
            x -= 1;
            edits.push(Edit { op: Op::Delete, a: x as usize, b: y as usize });
        }
    }
    edits.reverse();
    edits
}

/// Number of lines the script inserts plus deletes.
pub fn distance(edits: &[Edit]) -> usize {
    edits.iter().filter(|e| e.op != Op::Equal).count()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Hunk {
    pub a_start: usize,
    pub a_len: usize,
    pub b_start: usize,
    pub b_len: usize,
    pub lines: Vec<(char, String)>,
}

/// Groups an edit script into hunks with `context` unchanged lines around each change.
pub fn hunks(edits: &[Edit], a: &[&str], b: &[&str], context: usize) -> Vec<Hunk> {
    let changed: Vec<usize> = edits.iter().enumerate().filter(|(_, e)| e.op != Op::Equal).map(|(i, _)| i).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < changed.len() {
        // Extend the hunk while the next change is within 2 * context equal lines.
        let mut j = i;
        while j + 1 < changed.len() && changed[j + 1] - changed[j] - 1 <= 2 * context {
            j += 1;
        }
        let start = changed[i].saturating_sub(context);
        let end = (changed[j] + 1 + context).min(edits.len());
        let slice = &edits[start..end];
        let (mut a_len, mut b_len) = (0, 0);
        let mut lines = Vec::new();
        for e in slice {
            match e.op {
                Op::Equal => {
                    a_len += 1;
                    b_len += 1;
                    lines.push((' ', a[e.a].to_string()));
                }
                Op::Delete => {
                    a_len += 1;
                    lines.push(('-', a[e.a].to_string()));
                }
                Op::Insert => {
                    b_len += 1;
                    lines.push(('+', b[e.b].to_string()));
                }
            }
        }
        let first = slice[0];
        out.push(Hunk { a_start: first.a, a_len, b_start: first.b, b_len, lines });
        i = j + 1;
    }
    out
}

fn range(start: usize, len: usize) -> String {
    // A range of one line is just its number; an empty range points at the line before it.
    match len {
        0 => format!("{},0", start),
        1 => format!("{}", start + 1),
        _ => format!("{},{}", start + 1, len),
    }
}

/// The `@@ ... @@` lines of a unified diff of two texts (no file headers). Empty if equal.
pub fn unified(old: &str, new: &str, context: usize) -> String {
    let (a, b) = (split_lines(old), split_lines(new));
    let edits = diff(&a, &b);
    let mut out = String::new();
    for h in hunks(&edits, &a, &b, context) {
        let _ = writeln!(out, "@@ -{} +{} @@", range(h.a_start, h.a_len), range(h.b_start, h.b_len));
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

/// Whether content is treated as binary: a NUL byte in the first 8000 bytes, which is git's rule.
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// A hunk of change from the base: lines `base[start..end]` are replaced by `lines`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    start: usize,
    end: usize,
    lines: Vec<String>,
}

fn changes(base: &[&str], other: &[&str]) -> Vec<Change> {
    let edits = diff(base, other);
    let mut out: Vec<Change> = Vec::new();
    let mut cur: Option<Change> = None;
    for e in &edits {
        match e.op {
            Op::Equal => {
                if let Some(c) = cur.take() {
                    out.push(c);
                }
            }
            Op::Delete => {
                let c = cur.get_or_insert(Change { start: e.a, end: e.a, lines: vec![] });
                c.end = e.a + 1;
            }
            Op::Insert => {
                let c = cur.get_or_insert(Change { start: e.a, end: e.a, lines: vec![] });
                c.lines.push(other[e.b].to_string());
            }
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub struct Merged {
    pub text: String,
    pub conflicts: usize,
}

fn apply(base: &[&str], start: usize, end: usize, changes: &[&Change]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = start;
    for c in changes {
        out.extend(base[pos..c.start].iter().map(|s| s.to_string()));
        out.extend(c.lines.iter().cloned());
        pos = c.end;
    }
    out.extend(base[pos..end].iter().map(|s| s.to_string()));
    out
}

/// Merges two versions that both descend from `base`, line by line. Changes that do not touch
/// each other are combined; changes to the same or adjacent lines that differ are conflicts,
/// written with `<<<<<<<`, `=======` and `>>>>>>>` markers naming `ours_label` and `theirs_label`.
pub fn merge3(base: &str, ours: &str, theirs: &str, ours_label: &str, theirs_label: &str) -> Merged {
    let (b, o, t) = (split_lines(base), split_lines(ours), split_lines(theirs));
    let (co, ct) = (changes(&b, &o), changes(&b, &t));
    let mut all: Vec<(bool, &Change)> = co.iter().map(|c| (true, c)).chain(ct.iter().map(|c| (false, c))).collect();
    all.sort_by_key(|(_, c)| (c.start, c.end));

    let mut text = String::new();
    let mut conflicts = 0;
    let mut pos = 0;
    let mut i = 0;
    let push_lines = |text: &mut String, lines: &[String]| {
        for l in lines {
            text.push_str(l);
        }
    };
    while i < all.len() {
        // Gather the group of changes that touch each other.
        let (start, mut end) = (all[i].1.start, all[i].1.end);
        let mut j = i + 1;
        while j < all.len() && all[j].1.start <= end {
            end = end.max(all[j].1.end);
            j += 1;
        }
        let group = &all[i..j];
        for l in &b[pos..start] {
            text.push_str(l);
        }
        let ours_changes: Vec<&Change> = group.iter().filter(|(is_ours, _)| *is_ours).map(|(_, c)| *c).collect();
        let theirs_changes: Vec<&Change> = group.iter().filter(|(is_ours, _)| !*is_ours).map(|(_, c)| *c).collect();
        let (our_text, their_text) = (apply(&b, start, end, &ours_changes), apply(&b, start, end, &theirs_changes));
        if theirs_changes.is_empty() {
            push_lines(&mut text, &our_text);
        } else if ours_changes.is_empty() || our_text == their_text {
            push_lines(&mut text, &their_text);
        } else {
            conflicts += 1;
            text.push_str(&format!("<<<<<<< {ours_label}\n"));
            push_lines(&mut text, &our_text);
            if our_text.last().is_some_and(|l| !l.ends_with('\n')) {
                text.push('\n');
            }
            text.push_str("=======\n");
            push_lines(&mut text, &their_text);
            if their_text.last().is_some_and(|l| !l.ends_with('\n')) {
                text.push('\n');
            }
            text.push_str(&format!(">>>>>>> {theirs_label}\n"));
        }
        pos = end;
        i = j;
    }
    for l in &b[pos..] {
        text.push_str(l);
    }
    Merged { text, conflicts }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Length of the longest common subsequence by dynamic programming: the oracle for minimality.
    fn lcs<T: PartialEq>(a: &[T], b: &[T]) -> usize {
        let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                dp[i][j] = if a[i - 1] == b[j - 1] { dp[i - 1][j - 1] + 1 } else { dp[i - 1][j].max(dp[i][j - 1]) };
            }
        }
        dp[a.len()][b.len()]
    }

    fn apply_script<T: Clone + PartialEq + std::fmt::Debug>(a: &[T], b: &[T], edits: &[Edit]) -> Vec<T> {
        let mut out = Vec::new();
        let (mut ai, mut bi) = (0, 0);
        for e in edits {
            match e.op {
                Op::Equal => {
                    assert_eq!((e.a, e.b), (ai, bi));
                    assert_eq!(a[ai], b[bi]);
                    out.push(a[ai].clone());
                    ai += 1;
                    bi += 1;
                }
                Op::Delete => {
                    assert_eq!(e.a, ai);
                    ai += 1;
                }
                Op::Insert => {
                    assert_eq!(e.b, bi);
                    out.push(b[bi].clone());
                    bi += 1;
                }
            }
        }
        assert_eq!((ai, bi), (a.len(), b.len()), "the script must consume both inputs");
        out
    }

    #[test]
    fn splitting_lines() {
        assert_eq!(split_lines(""), Vec::<&str>::new());
        assert_eq!(split_lines("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(split_lines("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_lines("\n\n"), vec!["\n", "\n"]);
        assert_eq!(split_lines("a\r\nb"), vec!["a\r\n", "b"]);
    }

    #[test]
    fn small_cases() {
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("", "abc"),
            ("abc", ""),
            ("abc", "abc"),
            ("abc", "abd"),
            ("abcabba", "cbabac"),
            ("a", "b"),
            ("ab", "ba"),
            ("aaaa", "aa"),
            ("xaxbx", "ab"),
        ];
        for (a, b) in cases {
            let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
            let e = diff(&a, &b);
            assert_eq!(apply_script(&a, &b, &e), b);
            assert_eq!(a.len() + b.len() - 2 * lcs(&a, &b), distance(&e), "not minimal for {a:?} -> {b:?}");
        }
    }

    #[test]
    fn random_inputs_give_minimal_correct_scripts() {
        let mut seed = 0xdeadbeefu64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for round in 0..3000 {
            let alphabet = 2 + next(5);
            let a: Vec<u8> = (0..next(30)).map(|_| next(alphabet) as u8).collect();
            // b is a mutation of a half the time, an unrelated sequence the other half.
            let b: Vec<u8> = if round % 2 == 0 {
                let mut v = a.clone();
                for _ in 0..next(5) {
                    if !v.is_empty() && next(2) == 0 {
                        v.remove(next(v.len() as u64) as usize);
                    } else {
                        let at = next(v.len() as u64 + 1) as usize;
                        v.insert(at, next(alphabet) as u8);
                    }
                }
                v
            } else {
                (0..next(30)).map(|_| next(alphabet) as u8).collect()
            };
            let e = diff(&a, &b);
            assert_eq!(apply_script(&a, &b, &e), b, "{a:?} -> {b:?}");
            assert_eq!(a.len() + b.len() - 2 * lcs(&a, &b), distance(&e), "not minimal for {a:?} -> {b:?}");
        }
    }

    #[test]
    fn large_inputs_with_few_changes_are_fast() {
        let a: Vec<u32> = (0..200_000).collect();
        let mut b = a.clone();
        b.remove(100_000);
        b.insert(50_000, 999_999);
        b[150_000] = 888_888;
        let t = std::time::Instant::now();
        let e = diff(&a, &b);
        assert_eq!(distance(&e), 4);
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn unified_format() {
        let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        let new = "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\n";
        let u = unified(old, new, 3);
        assert_eq!(
            u,
            "@@ -1,6 +1,6 @@\n one\n two\n-three\n+THREE\n four\n five\n six\n@@ -8,3 +8,4 @@\n eight\n nine\n ten\n+eleven\n"
        );
        assert_eq!(unified("a\n", "a\n", 3), "");
    }

    #[test]
    fn unified_ranges_for_empty_and_single_line_files() {
        assert_eq!(unified("", "x\n", 3), "@@ -0,0 +1 @@\n+x\n");
        assert_eq!(unified("x\n", "", 3), "@@ -1 +0,0 @@\n-x\n");
        assert_eq!(unified("a\n", "b\n", 3), "@@ -1 +1 @@\n-a\n+b\n");
    }

    #[test]
    fn missing_final_newline_is_marked() {
        assert_eq!(unified("a\nb", "a\nb\n", 3), "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n");
        assert_eq!(unified("a\n", "a", 3), "@@ -1 +1 @@\n-a\n+a\n\\ No newline at end of file\n");
    }

    #[test]
    fn hunks_merge_when_the_context_overlaps() {
        let old: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let mut lines: Vec<String> = (1..=30).map(|i| format!("{i}\n")).collect();
        lines[4] = "five\n".into();
        lines[10] = "eleven\n".into(); // 5 equal lines between: within 2 * 3, one hunk
        lines[25] = "twenty-six\n".into();
        let u = unified(&old, &lines.concat(), 3);
        assert_eq!(u.matches("@@ -").count(), 2, "{u}");
    }

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text\n"));
        let mut late = vec![b'a'; 9000];
        late.push(0);
        assert!(!is_binary(&late), "only the first 8000 bytes count");
    }

    fn m(base: &str, ours: &str, theirs: &str) -> Merged {
        merge3(base, ours, theirs, "ours", "theirs")
    }

    #[test]
    fn merge_combines_changes_that_do_not_touch() {
        let base = "a\nb\nc\nd\ne\nf\n";
        let r = m(base, "A\nb\nc\nd\ne\nf\n", "a\nb\nc\nd\ne\nF\n");
        assert_eq!(r, Merged { text: "A\nb\nc\nd\ne\nF\n".into(), conflicts: 0 });
        // One side only.
        assert_eq!(m(base, base, "a\nB\nc\nd\ne\nf\n").text, "a\nB\nc\nd\ne\nf\n");
        assert_eq!(m(base, "a\nB\nc\nd\ne\nf\n", base).text, "a\nB\nc\nd\ne\nf\n");
        // Both made the same change.
        assert_eq!(
            m(base, "a\nX\nc\nd\ne\nf\n", "a\nX\nc\nd\ne\nf\n"),
            Merged { text: "a\nX\nc\nd\ne\nf\n".into(), conflicts: 0 }
        );
        // An insertion by each side, far apart.
        assert_eq!(m(base, "new\na\nb\nc\nd\ne\nf\n", "a\nb\nc\nd\ne\nf\nend\n").text, "new\na\nb\nc\nd\ne\nf\nend\n");
    }

    #[test]
    fn merge_conflicts_on_the_same_or_adjacent_lines() {
        let base = "a\nb\nc\nd\n";
        let r = m(base, "a\nB1\nc\nd\n", "a\nB2\nc\nd\n");
        assert_eq!(r.conflicts, 1);
        assert_eq!(r.text, "a\n<<<<<<< ours\nB1\n=======\nB2\n>>>>>>> theirs\nc\nd\n");
        // Adjacent lines count as touching, like git.
        let r = m(base, "a\nB\nc\nd\n", "a\nb\nC\nd\n");
        assert_eq!(r.conflicts, 1);
        // Delete against modify.
        let r = m(base, "a\nc\nd\n", "a\nB\nc\nd\n");
        assert_eq!(r.conflicts, 1);
        assert!(r.text.contains("<<<<<<< ours\n=======\nB\n>>>>>>> theirs"), "{}", r.text);
        // Two separate conflicts.
        let r = m("1\n2\n3\n4\n5\n", "x\n2\n3\n4\ny\n", "z\n2\n3\n4\nw\n");
        assert_eq!(r.conflicts, 2);
    }

    #[test]
    fn merge_handles_missing_final_newlines_and_empty_inputs() {
        let r = m("a\nb", "a\nB1", "a\nB2");
        assert_eq!(r.text, "a\n<<<<<<< ours\nB1\n=======\nB2\n>>>>>>> theirs\n");
        assert_eq!(m("", "x\n", "x\n"), Merged { text: "x\n".into(), conflicts: 0 });
        assert_eq!(m("", "x\n", "y\n").conflicts, 1);
        assert_eq!(m("a\n", "", "a\n").text, "");
        assert_eq!(m("", "", "").conflicts, 0);
    }

    /// A merge in which only one side changed must equal that side, whatever the change is.
    #[test]
    fn merging_with_an_unchanged_side_returns_the_other() {
        let mut seed = 77u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..500 {
            let base: Vec<String> = (0..next(15)).map(|_| format!("{}\n", next(6))).collect();
            let other: Vec<String> = (0..next(15)).map(|_| format!("{}\n", next(6))).collect();
            let (b, o) = (base.concat(), other.concat());
            assert_eq!(m(&b, &b, &o), Merged { text: o.clone(), conflicts: 0 });
            assert_eq!(m(&b, &o, &b), Merged { text: o, conflicts: 0 });
        }
    }
}
