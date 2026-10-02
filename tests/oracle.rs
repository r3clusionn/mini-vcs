//! Checks `mg` against real git: the same operations must give the same object ids, git must
//! accept every repository `mg` writes, and `mg` must read every repository git writes.
//! Skipped (loudly) when `git` is not installed.

mod common;

use common::*;

macro_rules! need_git {
    () => {
        if !git_available() {
            eprintln!("SKIPPED: git is not installed");
            return;
        }
    };
}

fn git_ok(s: &Sandbox, args: &[&str]) -> String {
    let o = s.git(args);
    assert_eq!(o.code, 0, "git {args:?} failed: {}{}", o.stdout, o.stderr);
    o.stdout
}

#[test]
fn hash_object_agrees_with_git() {
    need_git!();
    let s = Sandbox::new();
    s.init();
    let big: Vec<u8> = (0..2_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    let samples: Vec<(&str, Vec<u8>)> = vec![
        ("empty", vec![]),
        ("one", b"x".to_vec()),
        ("text", b"hello\nworld\n".to_vec()),
        ("crlf", b"a\r\nb\r\n".to_vec()),
        ("nul", vec![0, 1, 2, 0, 255]),
        ("utf8", "caf\u{e9} \u{65e5}\u{672c}\n".as_bytes().to_vec()),
        ("block", vec![b'a'; 64]),
        ("block-1", vec![b'a'; 63]),
        ("block+1", vec![b'a'; 65]),
        ("big", big),
    ];
    for (name, data) in samples {
        s.write("sample.bin", &data);
        let mine = s.mg(&["hash-object", "sample.bin"]).ok().out().to_string();
        let theirs = git_ok(&s, &["hash-object", "--no-filters", "sample.bin"]);
        assert_eq!(mine, theirs.trim(), "{name}");
    }
}

#[test]
fn identical_operations_give_identical_commit_ids() {
    need_git!();
    // A simpler build that both tools support the same way (`add -A` is spelled `add .` in mg).
    let steps = |s: &Sandbox, tool: &str| -> Vec<String> {
        let run = |args: &[&str], date: &str| {
            let o = if tool == "git" { s.git_dated(date, args) } else { s.mg_dated(std::path::Path::new(""), date, args) };
            assert_eq!(o.code, 0, "{tool} {args:?}: {}{}", o.stdout, o.stderr);
        };
        let rev = |r: &str| -> String {
            if tool == "git" {
                s.git(&["rev-parse", r]).stdout.trim().to_string()
            } else {
                s.mg(&["rev-parse", r]).out().to_string()
            }
        };
        let mut ids = Vec::new();
        run(if tool == "git" { &["init", "-q", "-b", "main"] } else { &["init"] }, "1700000000 +0000");
        s.write("README.md", "# project\n");
        s.write("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
        s.write("src/lib/util.rs", "pub fn f() {}\n");
        s.write("empty.txt", "");
        s.write("docs/a b/space.txt", "spaces in names\n");
        s.write("caf\u{e9}/\u{65e5}\u{672c}.txt", "unicode names\n");
        run(&["add", "."], "1700000010 +0000");
        run(&["commit", "-m", "initial commit"], "1700000020 +0000");
        ids.push(rev("HEAD"));

        s.write("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
        s.write("CHANGELOG.md", "- first\n");
        run(&["add", "src/main.rs", "CHANGELOG.md"], "1700000030 +0000");
        run(&["commit", "-m", "second\n\nA body that has several lines.\nAnd a second line.\n"], "1700000040 +0100");
        ids.push(rev("HEAD"));

        run(&["branch", "feature"], "1700000050 +0000");
        run(&["checkout", "feature"], "1700000050 +0000");
        s.write("feature.txt", "feature\n");
        s.remove("README.md");
        run(&["add", "."], "1700000060 +0000");
        run(&["commit", "-a", "-m", "feature work"], "1700000070 -0500");
        ids.push(rev("HEAD"));

        run(&["checkout", "main"], "1700000080 +0000");
        s.write("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n// more\n");
        run(&["commit", "-a", "-m", "main moves"], "1700000090 +0000");
        ids.push(rev("HEAD"));

        // A clean three-way merge. git needs --no-edit and --no-ff semantic parity: both create a merge commit here.
        if tool == "git" {
            run(&["merge", "--no-edit", "feature"], "1700000100 +0000");
        } else {
            run(&["merge", "feature"], "1700000100 +0000");
        }
        ids.push(rev("HEAD"));
        ids.push(rev("HEAD^{tree}"));

        run(&["tag", "v1"], "1700000110 +0000");
        run(&["tag", "-a", "v2", "-m", "release two"], "1700000120 +0000");
        ids.push(rev("v1"));
        ids.push(rev("v2"));
        ids.push(rev("v2^{commit}"));
        ids
    };
    let (g, m) = (Sandbox::new(), Sandbox::new());
    let git_ids = steps(&g, "git");
    let mg_ids = steps(&m, "mg");
    assert_eq!(git_ids, mg_ids, "commit, tree and tag ids must be identical");
    // Not a trivial comparison: all those ids are different from each other.
    let mut uniq = git_ids.clone();
    uniq.sort();
    uniq.dedup();
    assert!(uniq.len() >= 7);
}

#[test]
fn git_accepts_what_mg_writes() {
    need_git!();
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.write("dir/b.txt", "b\n");
    s.write("dir/sub/c.txt", "c\n");
    s.write("empty", "");
    s.commit_all("one");
    s.mg(&["checkout", "-b", "topic"]).ok();
    s.write("dir/b.txt", "b2\n");
    s.write("topic.txt", "t\n");
    s.commit_all("topic work");
    s.mg(&["checkout", "main"]).ok();
    s.write("a.txt", "a2\n");
    s.commit_all("main work");
    assert_eq!(s.mg(&["merge", "topic"]).code, 0);
    s.mg(&["tag", "-a", "v1", "-m", "annotated"]).ok();
    s.mg(&["tag", "light"]).ok();
    s.write("dir/b.txt", "unstaged edit\n");
    s.write("staged.txt", "s\n");
    s.mg(&["add", "staged.txt"]).ok();
    s.write("untracked.txt", "u\n");

    // git's own consistency check.
    let fsck = s.git(&["fsck", "--strict", "--no-dangling"]);
    assert_eq!(fsck.code, 0, "{}{}", fsck.stdout, fsck.stderr);
    assert!(!fsck.stderr.contains("error") && !fsck.stdout.contains("error"), "{}{}", fsck.stdout, fsck.stderr);
    // The same history.
    assert_eq!(git_ok(&s, &["log", "--format=%H", "main"]), s.mg(&["log", "--format=%H"]).ok().stdout);
    assert_eq!(git_ok(&s, &["rev-parse", "v1^{commit}"]).trim(), s.mg(&["rev-parse", "v1^{commit}"]).out());
    assert_eq!(git_ok(&s, &["tag", "-l"]), s.mg(&["tag"]).ok().stdout);
    assert_eq!(
        git_ok(&s, &["branch", "--format=%(refname:short)"]),
        s.mg(&["branch"]).ok().stdout.replace("* ", "").replace("  ", "")
    );
    // The same index and the same status.
    assert_eq!(git_ok(&s, &["ls-files", "-s"]), s.mg(&["ls-files", "-s"]).ok().stdout);
    assert_eq!(git_ok(&s, &["status", "--porcelain"]), s.mg(&["status", "--porcelain"]).ok().stdout);
    // And git can work with it: commit the staged file with git, then mg sees a clean state.
    git_ok(&s, &["commit", "-m", "committed by git"]);
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), " M dir/b.txt\n?? untracked.txt");
    assert!(s.mg(&["log", "-n", "1"]).stdout.contains("committed by git"));
    assert_eq!(s.mg(&["fsck"]).code, 0);
}

#[test]
fn mg_reads_what_git_writes() {
    need_git!();
    let s = Sandbox::new();
    git_ok(&s, &["init", "-q", "-b", "main"]);
    s.write("a.txt", "alpha\n");
    s.write("run.sh", "#!/bin/sh\n");
    s.write("dir/b.txt", "bravo\n");
    s.write("empty", "");
    s.write("no-newline", "no newline at the end");
    git_ok(&s, &["add", "."]);
    git_ok(&s, &["update-index", "--chmod=+x", "run.sh"]);
    git_ok(&s, &["commit", "-q", "-m", "first\n\nbody here"]);
    s.write("a.txt", "alpha\nbeta\n");
    git_ok(&s, &["commit", "-q", "-a", "-m", "second"]);
    git_ok(&s, &["checkout", "-q", "-b", "side"]);
    s.write("side.txt", "s\n");
    git_ok(&s, &["add", "."]);
    git_ok(&s, &["commit", "-q", "-m", "side"]);
    git_ok(&s, &["checkout", "-q", "main"]);
    s.write("main.txt", "m\n");
    git_ok(&s, &["add", "."]);
    git_ok(&s, &["commit", "-q", "-m", "main"]);
    git_ok(&s, &["merge", "-q", "--no-edit", "side"]);
    git_ok(&s, &["tag", "-a", "v1", "-m", "tag message"]);

    assert!(s.mg(&["fsck"]).ok().stdout.contains("0 problems"));
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), git_ok(&s, &["status", "--porcelain"]).trim_end());
    assert_eq!(s.mg(&["log"]).ok().stdout, git_ok(&s, &["log"]), "the default log format matches git's exactly, merges included");
    assert_eq!(s.mg(&["log", "--oneline"]).ok().stdout, git_ok(&s, &["log", "--oneline"]));
    assert_eq!(s.mg(&["ls-files", "-s"]).ok().stdout, git_ok(&s, &["ls-files", "-s"]));
    assert_eq!(s.mg(&["ls-tree", "-r", "HEAD"]).ok().stdout, git_ok(&s, &["ls-tree", "-r", "HEAD"]));
    assert_eq!(s.mg(&["ls-tree", "HEAD"]).ok().stdout, git_ok(&s, &["ls-tree", "HEAD"]));
    assert_eq!(s.mg(&["rev-parse", "v1^{commit}"]).out(), git_ok(&s, &["rev-parse", "v1^{commit}"]).trim());
    assert_eq!(s.mg(&["rev-parse", "HEAD~2"]).out(), git_ok(&s, &["rev-parse", "HEAD~2"]).trim());
    assert_eq!(s.mg(&["rev-parse", "HEAD^2"]).out(), git_ok(&s, &["rev-parse", "HEAD^2"]).trim());
    assert_eq!(s.mg(&["diff", "HEAD~1", "HEAD"]).ok().stdout, git_ok(&s, &["diff", "HEAD~1", "HEAD"]));
    assert_eq!(s.mg(&["diff", "main~3", "main"]).ok().stdout, git_ok(&s, &["diff", "main~3", "main"]));
    assert_eq!(s.mg(&["cat-file", "-p", "v1"]).ok().stdout, git_ok(&s, &["cat-file", "-p", "v1"]));
    // The executable bit git recorded is kept.
    assert!(s.mg(&["ls-files", "-s"]).ok().stdout.contains("100755 "));
    // mg can continue the history and git is happy with it.
    s.write("more.txt", "more\n");
    s.mg(&["add", "."]).ok();
    s.mg(&["commit", "-m", "by mg"]).ok();
    assert_eq!(git_ok(&s, &["log", "-1", "--format=%s"]).trim(), "by mg");
    assert_eq!(s.git(&["fsck", "--strict"]).code, 0);
}

#[test]
fn show_matches_git() {
    need_git!();
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "one\ntwo\nthree\n");
    s.write("b.txt", "b\n");
    s.commit_all("first");
    s.write("a.txt", "one\n2\nthree\nfour\n");
    s.remove("b.txt");
    s.write("c.txt", "new file\n");
    s.write("empty.txt", "");
    s.commit_all("second\n\nWith a body.\n\nAnd a second paragraph.");
    assert_eq!(s.mg(&["show"]).ok().stdout, git_ok(&s, &["show"]));
    assert_eq!(s.mg(&["show", "HEAD~1"]).ok().stdout, git_ok(&s, &["show", "HEAD~1"]));
    assert_eq!(s.mg(&["log"]).ok().stdout, git_ok(&s, &["log"]));
    assert_eq!(s.mg(&["log", "--format=%h|%an|%ae|%s|%p"]).ok().stdout, git_ok(&s, &["log", "--format=%h|%an|%ae|%s|%p"]));
}

/// Applies a set of edits to the work tree.
fn scramble(s: &Sandbox) {
    s.write("mod.txt", "line1\nline2 CHANGED\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10 CHANGED\n");
    s.write("added.txt", "brand new\nfile\n");
    s.remove("gone.txt");
    s.write("nonl.txt", "now without final newline");
    s.write("bin.dat", [0u8, 1, 2, 255, 0, 7]);
    s.write("emptied.txt", "");
    s.write("sub/new-in-dir.txt", "x\n");
    s.write("sub/deeper/ins.txt", "a\nb\nINSERTED\nc\n");
}

fn seed_repo(s: &Sandbox) {
    s.init();
    s.write("mod.txt", "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n");
    s.write("gone.txt", "will vanish\n");
    s.write("nonl.txt", "had a final newline\n");
    s.write("bin.dat", [0u8, 1, 2, 3]);
    s.write("emptied.txt", "was not empty\n");
    s.write("sub/deeper/ins.txt", "a\nb\nc\n");
    s.write("same.txt", "unchanged\n");
    s.commit_all("seed");
}

#[test]
fn diff_output_matches_git_in_every_mode() {
    need_git!();
    let s = Sandbox::new();
    seed_repo(&s);
    scramble(&s);
    // Unstaged: new files are untracked, so they do not appear until added.
    assert_eq!(s.mg(&["diff"]).ok().stdout, git_ok(&s, &["diff"]));
    s.mg(&["add", "added.txt", "sub/new-in-dir.txt"]).ok();
    assert_eq!(s.mg(&["diff"]).ok().stdout, git_ok(&s, &["diff"]));
    assert_eq!(s.mg(&["diff", "--cached"]).ok().stdout, git_ok(&s, &["diff", "--cached"]));
    s.mg(&["add", "."]).ok();
    assert_eq!(s.mg(&["diff", "--cached"]).ok().stdout, git_ok(&s, &["diff", "--cached"]));
    assert_eq!(s.mg(&["diff"]).ok().stdout, "");
    assert_eq!(s.mg(&["diff", "--name-status", "--cached"]).ok().stdout, git_ok(&s, &["diff", "--cached", "--name-status"]));
    s.mg(&["commit", "-m", "scrambled"]).ok();
    assert_eq!(s.mg(&["diff", "HEAD~1", "HEAD"]).ok().stdout, git_ok(&s, &["diff", "HEAD~1", "HEAD"]));
    assert_eq!(s.mg(&["diff", "HEAD", "HEAD~1"]).ok().stdout, git_ok(&s, &["diff", "HEAD", "HEAD~1"]));
    // Limited to a path.
    assert_eq!(s.mg(&["diff", "HEAD~1", "HEAD", "--", "sub"]).ok().stdout, git_ok(&s, &["diff", "HEAD~1", "HEAD", "--", "sub"]));
}

#[test]
fn diff_of_many_random_edits_matches_git() {
    need_git!();
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut mismatched = Vec::new();
    for round in 0..150 {
        let s = Sandbox::new();
        s.init();
        let lines: Vec<String> = (0..next(60) + 1).map(|i| format!("line {i} {}\n", next(4))).collect();
        s.write("f.txt", lines.concat());
        s.commit_all("base");
        let mut new = lines.clone();
        for _ in 0..next(6) + 1 {
            match next(3) {
                0 if !new.is_empty() => {
                    let i = next(new.len() as u64) as usize;
                    new.remove(i);
                }
                1 => {
                    let i = next(new.len() as u64 + 1) as usize;
                    new.insert(i, format!("inserted {}\n", next(5)));
                }
                _ if !new.is_empty() => {
                    let i = next(new.len() as u64) as usize;
                    new[i] = format!("changed {}\n", next(5));
                }
                _ => {}
            }
        }
        s.write("f.txt", new.concat());
        let (mine, theirs) = (s.mg(&["diff"]).ok().stdout, git_ok(&s, &["diff"]));
        if mine != theirs {
            mismatched.push(round);
        }
        // Whatever the layout of the hunks, applying mg's diff with git must reproduce the new file.
        std::fs::write(s.path().join("patch.diff"), &mine).unwrap();
        s.write("f.txt", lines.concat());
        let apply = s.git(&["apply", "patch.diff"]);
        assert_eq!(apply.code, 0, "round {round}: git could not apply mg's diff: {}", apply.stderr);
        assert_eq!(s.read("f.txt"), new.concat(), "round {round}");
    }
    // git's diff has tie-breaking heuristics (it slides ambiguous hunks); identical output is expected
    // for the large majority and every diff must apply, which was asserted above.
    assert!(mismatched.is_empty(), "{} of 150 random diffs differ textually from git: {mismatched:?}", mismatched.len());
}

#[test]
fn ignore_rules_give_the_same_untracked_list_as_git() {
    need_git!();
    let s = Sandbox::new();
    s.init();
    s.write(".gitignore", "# comment\n*.log\nbuild/\n/root-only.txt\n!important.log\ntemp*\n**/cache/\ndocs/**/*.tmp\nsrc/gen/\n!src/gen/keep.rs\n\\#hash\n*.o\n[Tt]est?.txt\n");
    s.write("sub/.gitignore", "local.txt\n!*.log\n/anchored\n");
    s.write("kept.txt", "k\n");
    s.commit_all("base");
    for f in [
        "a.log",
        "important.log",
        "build/out.o",
        "build.txt",
        "root-only.txt",
        "sub/root-only.txt",
        "temp1.txt",
        "sub/temp2/x",
        "x/cache/data",
        "cache/data",
        "docs/a/b/c.tmp",
        "docs/c.tmp",
        "src/gen/out.rs",
        "src/gen/keep.rs",
        "#hash",
        "main.o",
        "deep/er/main.o",
        "Test1.txt",
        "test2.txt",
        "tests.txt",
        "sub/local.txt",
        "sub/deeper/local.txt",
        "sub/err.log",
        "sub/anchored",
        "sub/deeper/anchored",
        "newfile.txt",
        "sub/newfile.txt",
    ] {
        s.write(f, "x\n");
    }
    let mine = s.mg(&["status", "--porcelain"]).ok().stdout;
    let theirs = git_ok(&s, &["status", "--porcelain", "--untracked-files=normal"]);
    assert_eq!(mine, theirs);
    // add . must stage exactly what git would stage.
    s.mg(&["add", "."]).ok();
    let mine_idx = s.mg(&["ls-files"]).ok().stdout;
    let s2 = Sandbox::new();
    let _ = s2; // a second sandbox is not needed: ask git about the same tree
    let git_idx = {
        git_ok(&s, &["reset", "-q"]);
        git_ok(&s, &["add", "."]);
        git_ok(&s, &["ls-files"])
    };
    assert_eq!(mine_idx, git_idx);
}

#[test]
fn merges_give_the_same_trees_and_conflicts_as_git() {
    need_git!();
    // (name, base, ours, theirs)
    let cases: &[(&str, &str, &str, &str)] = &[
        ("disjoint changes", "a\nb\nc\nd\ne\nf\ng\nh\n", "A\nb\nc\nd\ne\nf\ng\nh\n", "a\nb\nc\nd\ne\nf\ng\nH\n"),
        ("same change both", "a\nb\nc\n", "a\nX\nc\n", "a\nX\nc\n"),
        ("conflict", "a\nb\nc\n", "a\nOURS\nc\n", "a\nTHEIRS\nc\n"),
        ("adjacent lines", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nC\nd\n"),
        ("insertions apart", "1\n2\n3\n4\n5\n6\n", "0\n1\n2\n3\n4\n5\n6\n", "1\n2\n3\n4\n5\n6\n7\n"),
        ("delete vs edit elsewhere", "1\n2\n3\n4\n5\n6\n7\n8\n", "1\n2\n3\n5\n6\n7\n8\n", "1\n2\n3\n4\n5\n6\n7\n8 changed\n"),
        ("both add the file differently", "", "ours\n", "theirs\n"),
    ];
    for (name, base, ours, theirs) in cases {
        let run = |tool: &str| -> (i32, String, String) {
            let s = Sandbox::new();
            let t = |a: &[&str], d: &str| {
                let o = if tool == "git" { s.git_dated(d, a) } else { s.mg_dated(std::path::Path::new(""), d, a) };
                (o.code, o)
            };
            t(if tool == "git" { &["init", "-q", "-b", "main"] } else { &["init"] }, "1700000000 +0000");
            s.write("keep.txt", "keep\n");
            if !base.is_empty() {
                s.write("f.txt", base);
            }
            t(&["add", "."], "1700000001 +0000");
            t(&["commit", "-m", "base"], "1700000010 +0000");
            t(&["checkout", "-b", "other"], "1700000020 +0000");
            s.write("f.txt", theirs);
            t(&["add", "."], "1700000021 +0000");
            t(&["commit", "-m", "theirs"], "1700000030 +0000");
            t(&["checkout", "main"], "1700000040 +0000");
            s.write("f.txt", ours);
            t(&["add", "."], "1700000041 +0000");
            t(&["commit", "-m", "ours"], "1700000050 +0000");
            let args: Vec<&str> = if tool == "git" { vec!["merge", "--no-edit", "other"] } else { vec!["merge", "other"] };
            let (code, o) = t(&args, "1700000060 +0000");
            let _ = o;
            let status =
                if tool == "git" { s.git(&["status", "--porcelain"]).stdout } else { s.mg(&["status", "--porcelain"]).stdout };
            (code, s.read("f.txt"), status)
        };
        let (gc, gfile, gstatus) = run("git");
        let (mc, mfile, mstatus) = run("mg");
        assert_eq!(mc != 0, gc != 0, "{name}: conflict or not");
        assert_eq!(mfile, gfile, "{name}: merged file content");
        assert_eq!(mstatus, gstatus, "{name}: status after the merge");
    }
    // For the clean ones the resulting commit ids are identical too.
    let clean = |tool: &str| -> String {
        let s = Sandbox::new();
        let t = |a: &[&str], d: &str| {
            let o = if tool == "git" { s.git_dated(d, a) } else { s.mg_dated(std::path::Path::new(""), d, a) };
            assert_eq!(o.code, 0, "{tool} {a:?}: {}{}", o.stdout, o.stderr);
        };
        t(if tool == "git" { &["init", "-q", "-b", "main"] } else { &["init"] }, "1700000000 +0000");
        s.write("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
        t(&["add", "."], "1700000001 +0000");
        t(&["commit", "-m", "base"], "1700000010 +0000");
        t(&["checkout", "-b", "other"], "1700000020 +0000");
        s.write("f.txt", "1\n2\n3\n4\n5\n6\n7\n8 other\n");
        s.write("new.txt", "n\n");
        t(&["add", "."], "1700000021 +0000");
        t(&["commit", "-m", "theirs"], "1700000030 +0000");
        t(&["checkout", "main"], "1700000040 +0000");
        s.write("f.txt", "0 main\n1\n2\n3\n4\n5\n6\n7\n8\n");
        t(&["add", "."], "1700000041 +0000");
        t(&["commit", "-m", "ours"], "1700000050 +0000");
        t(if tool == "git" { &["merge", "--no-edit", "other"] } else { &["merge", "other"] }, "1700000060 +0000");
        if tool == "git" {
            s.git(&["rev-parse", "HEAD"]).stdout.trim().to_string()
        } else {
            s.mg(&["rev-parse", "HEAD"]).out().to_string()
        }
    };
    assert_eq!(clean("git"), clean("mg"));
}

/// Random operations on a work tree; after each step `mg` and `git` must report the same state.
#[test]
fn random_workload_status_matches_git() {
    need_git!();
    for seed in 1..=6u64 {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let s = Sandbox::new();
        s.init();
        let names = ["a.txt", "b.txt", "dir/c.txt", "dir/d.txt", "dir/sub/e.txt", "f.bin", "g/h/i.txt", "j.txt"];
        let mut commits = 0;
        for step in 0..45 {
            match next(8) {
                0 | 1 => {
                    let f = names[next(names.len() as u64) as usize];
                    s.write(f, format!("content {} {}\n", next(5), next(3)));
                }
                2 => {
                    let f = names[next(names.len() as u64) as usize];
                    if s.exists(f) {
                        s.remove(f);
                    }
                }
                3 => {
                    s.mg(&["add", "."]).ok();
                }
                4 => {
                    let f = names[next(names.len() as u64) as usize];
                    if s.exists(f) {
                        s.mg(&["add", f]).ok();
                    }
                }
                5 => {
                    let r = s.mg(&["commit", "-m", &format!("step {step}")]);
                    if r.code == 0 {
                        commits += 1;
                    }
                }
                6 if commits > 0 => {
                    let f = names[next(names.len() as u64) as usize];
                    s.mg(&["restore", "--staged", f]);
                }
                _ => {
                    s.mg(&["commit", "-a", "-m", &format!("all {step}")]);
                }
            }
            let mine = s.mg(&["status", "--porcelain"]).ok().stdout;
            let theirs = git_ok(&s, &["status", "--porcelain"]);
            assert_eq!(mine, theirs, "seed {seed}, step {step}");
            assert_eq!(s.mg(&["ls-files", "-s"]).ok().stdout, git_ok(&s, &["ls-files", "-s"]), "seed {seed}, step {step}: index");
        }
        assert_eq!(s.git(&["fsck", "--strict"]).code, 0, "seed {seed}");
        assert_eq!(s.mg(&["fsck"]).code, 0);
    }
}
