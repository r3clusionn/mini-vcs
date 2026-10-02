mod common;

use common::*;

#[test]
fn init_add_commit_log() {
    let s = Sandbox::new();
    s.init();
    assert!(s.exists(".git/HEAD") && s.exists(".git/objects") && s.exists(".git/refs/heads"));
    assert_eq!(s.read(".git/HEAD"), "ref: refs/heads/main\n");
    // Nothing yet.
    assert!(s.mg(&["log"]).stderr.contains("no commits"));
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "");
    assert!(s.mg(&["commit", "-m", "x"]).stderr.contains("nothing to commit"));

    s.write("hello.txt", "hello\n");
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "?? hello.txt");
    s.mg(&["add", "hello.txt"]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "A  hello.txt");
    let c = s.mg(&["commit", "-m", "first"]).ok();
    assert!(c.stdout.starts_with("[main (root-commit) "), "{}", c.stdout);
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "");
    let log = s.mg(&["log"]).ok();
    assert!(log.stdout.contains("Author: Tester <tester@example.com>") && log.stdout.contains("    first"));
    assert_eq!(s.mg(&["log", "--oneline"]).out().lines().count(), 1);

    // An unchanged tree is not a commit; an empty message is refused.
    assert!(s.mg(&["commit", "-m", "again"]).stderr.contains("nothing to commit"));
    s.write("hello.txt", "changed\n");
    s.mg(&["add", "."]).ok();
    assert!(s.mg(&["commit", "-m", "  \n "]).stderr.contains("empty commit message"));
    assert!(s.mg(&["commit"]).stderr.contains("message is required"));
    s.mg(&["commit", "-m", "second", "-m", "body paragraph"]).ok();
    let l = s.mg(&["log", "-n", "1"]).ok();
    assert!(l.stdout.contains("    second\n    \n    body paragraph"), "{}", l.stdout);
}

#[test]
fn add_stages_modifications_and_deletions_and_respects_ignore() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.write("b.txt", "b\n");
    s.write("dir/c.txt", "c\n");
    s.write("dir/sub/d.txt", "d\n");
    s.write("build/out.o", "obj\n");
    s.write("notes.log", "log\n");
    s.write(".gitignore", "build/\n*.log\n");
    s.commit_all("one");
    assert_eq!(s.mg(&["ls-files"]).out(), ".gitignore\na.txt\nb.txt\ndir/c.txt\ndir/sub/d.txt");

    s.write("a.txt", "a2\n");
    s.remove("b.txt");
    s.remove("dir/sub");
    s.write("new.txt", "n\n");
    s.write("build/more.o", "x\n");
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), " M a.txt\n D b.txt\n D dir/sub/d.txt\n?? new.txt");
    s.mg(&["add", "."]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "M  a.txt\nD  b.txt\nD  dir/sub/d.txt\nA  new.txt");

    // An ignored file is refused when named, and accepted with -f.
    let r = s.mg(&["add", "notes.log"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("ignored"));
    s.mg(&["add", "-f", "notes.log"]).ok();
    assert!(s.mg(&["ls-files"]).stdout.contains("notes.log"));
    // A path that matches nothing is an error.
    assert!(s.mg(&["add", "nothing-here"]).stderr.contains("did not match"));
}

#[test]
fn untracked_directories_are_collapsed_like_git() {
    let s = Sandbox::new();
    s.init();
    s.write("tracked/a.txt", "a\n");
    s.commit_all("one");
    s.write("tracked/new.txt", "n\n");
    s.write("fresh/x.txt", "x\n");
    s.write("fresh/deeper/y.txt", "y\n");
    std::fs::create_dir_all(s.path().join("empty-dir")).unwrap();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "?? fresh/\n?? tracked/new.txt");
}

#[test]
fn status_long_format() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    let t = s.mg(&["status"]).ok();
    assert!(
        t.stdout.contains("On branch main") && t.stdout.contains("No commits yet") && t.stdout.contains("Untracked files:"),
        "{}",
        t.stdout
    );
    s.commit_all("one");
    assert!(s.mg(&["status"]).stdout.contains("nothing to commit, working tree clean"));
    s.write("a.txt", "b\n");
    s.write("new.txt", "n\n");
    s.mg(&["add", "new.txt"]).ok();
    let t = s.mg(&["status"]).ok().stdout;
    assert!(t.contains("Changes to be committed:") && t.contains("new file:   new.txt"), "{t}");
    assert!(t.contains("Changes not staged for commit:") && t.contains("modified:   a.txt"), "{t}");
}

#[test]
fn commit_all_flag_stages_tracked_changes_only() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.write("gone.txt", "g\n");
    s.commit_all("one");
    s.write("a.txt", "a2\n");
    s.remove("gone.txt");
    s.write("untracked.txt", "u\n");
    s.mg(&["commit", "-a", "-m", "two"]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "?? untracked.txt");
    assert_eq!(s.mg(&["ls-files"]).out(), "a.txt");
}

#[test]
fn amend_rewrites_the_last_commit_keeping_its_author_date() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    let first = s.commit_all("original message");
    s.write("a.txt", "a, amended\n");
    s.mg(&["add", "."]).ok();
    s.mg(&["commit", "--amend", "-m", "better message"]).ok();
    let after = s.mg(&["rev-parse", "HEAD"]).ok().out().to_string();
    assert_ne!(first, after);
    assert_eq!(s.mg(&["log", "--oneline"]).out().lines().count(), 1, "the old commit is replaced, not added to");
    assert!(s.mg(&["log"]).stdout.contains("better message"));
    // Amending with no new message keeps the old one.
    s.write("b.txt", "b\n");
    s.mg(&["add", "."]).ok();
    s.mg(&["commit", "--amend"]).ok();
    assert!(s.mg(&["log"]).stdout.contains("better message"));
}

#[test]
fn rev_parse_forms() {
    let s = Sandbox::new();
    s.init();
    let mut ids = vec![];
    for i in 0..4 {
        s.write("f.txt", format!("{i}\n"));
        ids.push(s.commit_all(&format!("c{i}")));
    }
    let p = |r: &str| s.mg(&["rev-parse", r]).out().to_string();
    assert_eq!(p("HEAD"), ids[3]);
    assert_eq!(p("@"), ids[3]);
    assert_eq!(p("HEAD~1"), ids[2]);
    assert_eq!(p("HEAD^"), ids[2]);
    assert_eq!(p("HEAD~3"), ids[0]);
    assert_eq!(p("HEAD^^"), ids[1]);
    assert_eq!(p("HEAD~2^"), ids[0]);
    assert_eq!(p("main"), ids[3]);
    assert_eq!(p("main~2"), ids[1]);
    assert_eq!(p("refs/heads/main"), ids[3]);
    assert_eq!(p(&ids[1][..8]), ids[1]);
    assert_eq!(p(&ids[1]), ids[1]);
    assert_eq!(p("HEAD^{commit}"), ids[3]);
    assert_eq!(s.mg(&["cat-file", "-t", "HEAD^{tree}"]).out(), "tree");
    for bad in ["HEAD~4", "nonexistent", "HEAD^2", "zzzz", "HEAD~x", "HEAD^{blob}", "abcd"] {
        assert_ne!(s.mg(&["rev-parse", bad]).code, 0, "{bad}");
    }
}

#[test]
fn branches_and_tags() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    let c1 = s.commit_all("one");
    s.mg(&["branch", "feature"]).ok();
    s.mg(&["branch", "old", "HEAD"]).ok();
    assert_eq!(s.mg(&["branch"]).out(), "  feature\n* main\n  old");
    assert!(s.mg(&["branch", "feature"]).stderr.contains("already exists"));
    assert!(s.mg(&["branch", "bad name"]).code != 0);
    s.mg(&["branch", "-m", "old", "older"]).ok();
    assert!(s.mg(&["branch"]).stdout.contains("older"));
    s.mg(&["branch", "-d", "older"]).ok();
    assert!(s.mg(&["branch", "-d", "main"]).stderr.contains("checked out"));
    assert!(s.mg(&["branch", "-d", "nope"]).stderr.contains("no branch"));

    s.mg(&["tag", "v1"]).ok();
    s.mg(&["tag", "-a", "v2", "-m", "release two"]).ok();
    assert_eq!(s.mg(&["tag"]).out(), "v1\nv2");
    assert_eq!(s.mg(&["rev-parse", "v1"]).out(), c1);
    assert_ne!(s.mg(&["rev-parse", "v2"]).out(), c1, "an annotated tag is its own object");
    assert_eq!(s.mg(&["rev-parse", "v2^{commit}"]).out(), c1);
    assert_eq!(s.mg(&["cat-file", "-t", "v2"]).out(), "tag");
    assert!(s.mg(&["tag", "v1"]).stderr.contains("already exists"));
    assert!(s.mg(&["show", "v2"]).stdout.contains("release two"));
    s.mg(&["tag", "-d", "v1"]).ok();
    assert_eq!(s.mg(&["tag"]).out(), "v2");

    // A branch that has commits the current one lacks is "not fully merged".
    s.mg(&["checkout", "feature"]).ok();
    s.write("f.txt", "f\n");
    s.commit_all("feature work");
    s.mg(&["checkout", "main"]).ok();
    assert!(s.mg(&["branch", "-d", "feature"]).stderr.contains("not fully merged"));
    s.mg(&["branch", "-D", "feature"]).ok();
}

#[test]
fn cat_file_hash_object_and_ls_tree() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "hello\n");
    s.write("dir/b.txt", "bee\n");
    s.commit_all("one");
    assert_eq!(s.mg(&["hash-object", "a.txt"]).out(), "ce013625030ba8dba906f756967f9e9ca394464a");
    assert!(s.mg(&["cat-file", "-p", "HEAD:a.txt"]).code != 0, "HEAD:path is not supported, and says so");
    let tree = s.mg(&["ls-tree", "HEAD"]).ok().stdout;
    assert!(tree.contains("100644 blob ce013625030ba8dba906f756967f9e9ca394464a\ta.txt"), "{tree}");
    assert!(tree.contains("040000 tree "), "{tree}");
    let rec = s.mg(&["ls-tree", "-r", "HEAD"]).ok().stdout;
    assert!(rec.contains("\tdir/b.txt"));
    let blob = s
        .mg(&["ls-tree", "HEAD"])
        .ok()
        .stdout
        .lines()
        .find(|l| l.contains("a.txt"))
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .to_string();
    assert_eq!(s.mg(&["cat-file", "-p", &blob]).stdout, "hello\n");
    assert_eq!(s.mg(&["cat-file", "-s", &blob]).out(), "6");
    // -w stores the object.
    s.write("new.txt", "stored by hash-object\n");
    let id = s.mg(&["hash-object", "-w", "new.txt"]).ok().out().to_string();
    assert_eq!(s.mg(&["cat-file", "-p", &id]).stdout, "stored by hash-object\n");
}
