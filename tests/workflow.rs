mod common;

use common::*;
use minivcs::object::{Commit, Kind, Signature, Tree, TreeEntry, MODE_DIR, MODE_FILE};
use minivcs::repo::Repo;

/// main: a.txt, shared.txt; feature branches off with its own changes.
fn two_branches() -> Sandbox {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.write("shared.txt", "one\ntwo\nthree\n");
    s.commit_all("base");
    s.mg(&["checkout", "-b", "feature"]).ok();
    s.write("feature.txt", "feature only\n");
    s.write("dir/nested.txt", "nested\n");
    s.remove("a.txt");
    s.write("shared.txt", "one\ntwo\nthree\nfour\n");
    s.commit_all("feature work");
    s.mg(&["checkout", "main"]).ok();
    s
}

#[test]
fn checkout_switches_files_both_ways() {
    let s = two_branches();
    assert!(s.exists("a.txt") && !s.exists("feature.txt") && !s.exists("dir"));
    assert_eq!(s.read("shared.txt"), "one\ntwo\nthree\n");
    let out = s.mg(&["checkout", "feature"]).ok();
    assert!(out.stdout.contains("Switched to branch 'feature'"));
    assert!(!s.exists("a.txt") && s.exists("feature.txt") && s.read("dir/nested.txt") == "nested\n");
    assert_eq!(s.read("shared.txt"), "one\ntwo\nthree\nfour\n");
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "");
    s.mg(&["checkout", "main"]).ok();
    assert!(s.exists("a.txt") && !s.exists("feature.txt") && !s.exists("dir"), "emptied directories are removed");
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "");
    assert!(s.mg(&["checkout", "no-such-branch"]).code != 0);
}

#[test]
fn checkout_refuses_to_lose_local_changes() {
    let s = two_branches();
    // An unstaged change to a file that differs between the branches.
    s.write("shared.txt", "my edit\n");
    let r = s.mg(&["checkout", "feature"]);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("shared.txt") && r.stderr.contains("overwritten"), "{}", r.stderr);
    assert_eq!(s.read("shared.txt"), "my edit\n", "nothing was touched");
    assert!(!s.exists("feature.txt"));
    assert_eq!(s.mg(&["branch"]).out().lines().find(|l| l.starts_with('*')).unwrap(), "* main");

    // A staged change is protected too.
    s.mg(&["add", "shared.txt"]).ok();
    assert_ne!(s.mg(&["checkout", "feature"]).code, 0);

    // A change to a file that is the same on both branches comes along.
    s.mg(&["restore", "--staged", "shared.txt"]).ok();
    s.write("shared.txt", "one\ntwo\nthree\n");
    s.write("a.txt", "a, edited\n");
    s.write("untouched-by-both.txt", "x\n");
    // a.txt is deleted on `feature`, so editing it blocks the switch.
    assert_ne!(s.mg(&["checkout", "feature"]).code, 0);
    s.write("a.txt", "a\n");
    // An untracked file is carried along when the target does not have it...
    s.mg(&["checkout", "feature"]).ok();
    assert!(s.exists("untouched-by-both.txt"));
    // ...but refused when the target would overwrite it.
    s.mg(&["checkout", "main"]).ok();
    s.write("feature.txt", "my own untracked feature.txt\n");
    let r = s.mg(&["checkout", "feature"]);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("untracked") && r.stderr.contains("feature.txt"), "{}", r.stderr);
    assert_eq!(s.read("feature.txt"), "my own untracked feature.txt\n");
    // --force overrides.
    s.mg(&["checkout", "--force", "feature"]).ok();
    assert_eq!(s.read("feature.txt"), "feature only\n");
}

#[test]
fn checkout_to_a_commit_detaches_head() {
    let s = two_branches();
    let base = s.mg(&["rev-parse", "main"]).ok().out().to_string();
    s.write("later.txt", "l\n");
    s.commit_all("later");
    let out = s.mg(&["checkout", &base]).ok();
    assert!(out.stdout.contains("HEAD is now at"), "{}", out.stdout);
    assert!(!s.exists("later.txt"));
    assert!(s.read(".git/HEAD").trim() == base);
    assert!(s.mg(&["status"]).stdout.contains("HEAD detached at"));
    // Commits made now are on no branch, and can be named by a new branch.
    s.write("detached.txt", "d\n");
    s.commit_all("on a detached head");
    let tip = s.mg(&["rev-parse", "HEAD"]).ok().out().to_string();
    s.mg(&["branch", "rescued"]).ok();
    s.mg(&["checkout", "main"]).ok();
    assert_eq!(s.mg(&["rev-parse", "rescued"]).out(), tip);
    assert!(!s.exists("detached.txt"));
}

#[test]
fn checkout_paths_and_restore() {
    let s = two_branches();
    s.write("a.txt", "scribbled\n");
    s.write("shared.txt", "scribbled too\n");
    s.mg(&["checkout", "--", "a.txt"]).ok();
    assert_eq!(s.read("a.txt"), "a\n");
    assert_eq!(s.read("shared.txt"), "scribbled too\n");
    s.mg(&["restore", "shared.txt"]).ok();
    assert_eq!(s.read("shared.txt"), "one\ntwo\nthree\n");
    // From another commit.
    s.mg(&["restore", "--source", "feature", "shared.txt"]).ok();
    assert_eq!(s.read("shared.txt"), "one\ntwo\nthree\nfour\n");
    assert_eq!(
        s.mg(&["status", "--porcelain"]).out(),
        "M  shared.txt",
        "restoring from a commit stages it, like checkout <rev> -- path"
    );
    assert!(s.mg(&["restore", "nothing-like-this"]).code != 0);
    // A deleted file comes back.
    s.mg(&["reset", "--hard"]).ok();
    s.remove("a.txt");
    s.mg(&["restore", "a.txt"]).ok();
    assert_eq!(s.read("a.txt"), "a\n");
    // Directories restore recursively.
    s.mg(&["checkout", "feature"]).ok();
    s.remove("dir");
    s.mg(&["restore", "dir"]).ok();
    assert_eq!(s.read("dir/nested.txt"), "nested\n");
}

#[test]
fn reset_soft_mixed_hard() {
    let s = Sandbox::new();
    s.init();
    s.write("f.txt", "1\n");
    let c1 = s.commit_all("c1");
    s.write("f.txt", "2\n");
    s.write("g.txt", "g\n");
    let c2 = s.commit_all("c2");

    s.mg(&["reset", "--soft", &c1]).ok();
    assert_eq!(s.mg(&["rev-parse", "HEAD"]).out(), c1);
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "M  f.txt\nA  g.txt", "soft keeps the index");
    assert_eq!(s.read("f.txt"), "2\n");
    s.mg(&["commit", "-m", "c2 again"]).ok();

    s.mg(&["reset", "--mixed", &c1]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), " M f.txt\n?? g.txt", "mixed unstages but keeps files");
    assert_eq!(s.read("f.txt"), "2\n");

    s.write("untracked.txt", "u\n");
    s.mg(&["reset", "--hard", &c2]).ok();
    assert_eq!(s.read("f.txt"), "2\n");
    assert!(s.exists("g.txt"));
    s.write("f.txt", "scribble\n");
    s.mg(&["add", "f.txt"]).ok();
    s.mg(&["reset", "--hard"]).ok();
    assert_eq!(s.read("f.txt"), "2\n");
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "?? untracked.txt", "hard leaves untracked files");
    s.mg(&["reset", "--hard", &c1]).ok();
    assert!(!s.exists("g.txt"));
    assert!(s.mg(&["reset", "--soft", "--hard"]).code != 0);
    // Unstaging a path.
    s.write("f.txt", "again\n");
    s.mg(&["add", "f.txt"]).ok();
    s.mg(&["reset", "--", "f.txt"]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), " M f.txt\n?? untracked.txt");
}

#[test]
fn merge_fast_forward() {
    let s = two_branches();
    let feature = s.mg(&["rev-parse", "feature"]).ok().out().to_string();
    let r = s.mg(&["merge", "feature"]).ok();
    assert!(r.stdout.contains("Merge made") || r.stdout.contains("Fast-forward"), "{}", r.stdout);
    // main diverged? No: main has not moved since feature branched, but feature deleted a.txt, so a fast-forward applies.
    assert_eq!(s.mg(&["rev-parse", "HEAD"]).out(), feature);
    assert!(s.exists("feature.txt") && !s.exists("a.txt"));
    assert!(s.mg(&["merge", "feature"]).stdout.contains("Already up to date"));
}

#[test]
fn merge_clean_three_way_creates_a_two_parent_commit() {
    let s = two_branches();
    s.write("main-only.txt", "m\n");
    s.write("shared.txt", "ZERO\none\ntwo\nthree\n");
    s.commit_all("main moves on");
    let before = s.mg(&["rev-parse", "HEAD"]).ok().out().to_string();
    let theirs = s.mg(&["rev-parse", "feature"]).ok().out().to_string();
    let r = s.mg(&["merge", "feature"]).ok();
    assert!(r.stdout.contains("Merge made commit"), "{}", r.stdout);
    let head = s.mg(&["rev-parse", "HEAD"]).ok().out().to_string();
    assert_eq!(s.mg(&["rev-parse", "HEAD^1"]).out(), before);
    assert_eq!(s.mg(&["rev-parse", "HEAD^2"]).out(), theirs);
    assert_ne!(head, before);
    // Both sets of changes are present.
    assert_eq!(s.read("shared.txt"), "ZERO\none\ntwo\nthree\nfour\n");
    assert!(s.exists("main-only.txt") && s.exists("feature.txt") && !s.exists("a.txt"));
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "");
    let log = s.mg(&["log", "-n", "1"]).ok().stdout;
    assert!(log.contains("Merge: ") && log.contains("Merge branch 'feature'"), "{log}");
}

#[test]
fn merge_conflict_markers_stages_and_resolution() {
    let s = Sandbox::new();
    s.init();
    s.write("f.txt", "line1\nline2\nline3\n");
    s.commit_all("base");
    s.mg(&["checkout", "-b", "other"]).ok();
    s.write("f.txt", "line1\nOTHER\nline3\n");
    s.write("added-by-them.txt", "t\n");
    s.commit_all("other");
    s.mg(&["checkout", "main"]).ok();
    s.write("f.txt", "line1\nMAIN\nline3\n");
    s.commit_all("main");

    let r = s.mg(&["merge", "other"]);
    assert_eq!(r.code, 1);
    assert!(r.stdout.contains("CONFLICT: f.txt"), "{}", r.stdout);
    assert_eq!(s.read("f.txt"), "line1\n<<<<<<< HEAD\nMAIN\n=======\nOTHER\n>>>>>>> other\nline3\n");
    assert_eq!(s.read("added-by-them.txt"), "t\n", "the clean part of the merge is applied");
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "A  added-by-them.txt\nUU f.txt");
    let stages = s.mg(&["ls-files", "-s"]).ok().stdout;
    assert!(stages.contains(" 1\tf.txt") && stages.contains(" 2\tf.txt") && stages.contains(" 3\tf.txt"), "{stages}");
    // Committing with an unresolved conflict is refused; so is another merge.
    assert!(s.mg(&["commit", "-m", "x"]).stderr.contains("unresolved"));
    assert!(s.mg(&["merge", "other"]).stderr.contains("already in progress"));
    assert!(s.mg(&["status"]).stdout.contains("both modified:   f.txt"));

    // Resolve and finish: the merge commit has both parents and the default message.
    s.write("f.txt", "line1\nRESOLVED\nline3\n");
    s.mg(&["add", "f.txt"]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "A  added-by-them.txt\nM  f.txt");
    s.mg(&["commit"]).ok();
    assert_eq!(s.mg(&["rev-parse", "HEAD^2"]).ok().out(), s.mg(&["rev-parse", "other"]).ok().out());
    assert!(s.mg(&["log", "-n", "1"]).stdout.contains("Merge branch 'other'"));
    assert!(!s.exists(".git/MERGE_HEAD"));
    assert_eq!(s.read("f.txt"), "line1\nRESOLVED\nline3\n");
}

#[test]
fn merge_abort_restores_everything() {
    let s = Sandbox::new();
    s.init();
    s.write("f.txt", "1\n2\n3\n");
    s.commit_all("base");
    s.mg(&["checkout", "-b", "other"]).ok();
    s.write("f.txt", "1\nO\n3\n");
    s.write("new.txt", "n\n");
    s.commit_all("other");
    s.mg(&["checkout", "main"]).ok();
    s.write("f.txt", "1\nM\n3\n");
    let head = s.commit_all("main");
    assert_eq!(s.mg(&["merge", "other"]).code, 1);
    s.mg(&["merge", "--abort"]).ok();
    assert_eq!(s.mg(&["rev-parse", "HEAD"]).out(), head);
    assert_eq!(s.read("f.txt"), "1\nM\n3\n");
    assert!(!s.exists("new.txt") && !s.exists(".git/MERGE_HEAD"));
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "");
    assert!(s.mg(&["merge", "--abort"]).stderr.contains("no merge"));
}

#[test]
fn merge_modify_delete_conflict() {
    let s = Sandbox::new();
    s.init();
    s.write("f.txt", "1\n2\n3\n");
    s.write("keep.txt", "k\n");
    s.commit_all("base");
    s.mg(&["checkout", "-b", "other"]).ok();
    s.remove("f.txt");
    s.commit_all("other deletes f");
    s.mg(&["checkout", "main"]).ok();
    s.write("f.txt", "1\nchanged\n3\n");
    s.commit_all("main edits f");
    assert_eq!(s.mg(&["merge", "other"]).code, 1);
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "UD f.txt");
    assert_eq!(s.read("f.txt"), "1\nchanged\n3\n", "the surviving version stays in the work tree");
}

#[test]
fn merge_refuses_with_local_changes_and_binary_conflicts_keep_ours() {
    let s = Sandbox::new();
    s.init();
    s.write("b.bin", [0u8, 1, 2, 3]);
    s.write("t.txt", "t\n");
    s.commit_all("base");
    s.mg(&["checkout", "-b", "other"]).ok();
    s.write("b.bin", [0u8, 9, 9, 9]);
    s.commit_all("other binary");
    s.mg(&["checkout", "main"]).ok();
    s.write("b.bin", [0u8, 5, 5, 5]);
    s.commit_all("main binary");
    s.write("t.txt", "local edit\n");
    assert!(s.mg(&["merge", "other"]).stderr.contains("local changes"));
    s.mg(&["checkout", "--", "t.txt"]).ok();
    assert_eq!(s.mg(&["merge", "other"]).code, 1);
    assert_eq!(std::fs::read(s.path().join("b.bin")).unwrap(), vec![0u8, 5, 5, 5]);
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "UU b.bin");
}

#[test]
fn rm_removes_from_index_and_disk() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.write("d/b.txt", "b\n");
    s.write("d/c.txt", "c\n");
    s.commit_all("one");
    s.mg(&["rm", "a.txt"]).ok();
    assert!(!s.exists("a.txt"));
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "D  a.txt");
    s.mg(&["rm", "--cached", "d/b.txt"]).ok();
    assert!(s.exists("d/b.txt"));
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "D  a.txt\nD  d/b.txt\n?? d/b.txt");
    s.write("d/c.txt", "edited\n");
    assert!(s.mg(&["rm", "d/c.txt"]).stderr.contains("local modifications"));
    s.mg(&["rm", "-f", "d"]).ok();
    assert!(!s.exists("d/c.txt"));
    assert!(s.mg(&["rm", "never-tracked"]).code != 0);
}

#[test]
fn commands_work_from_a_subdirectory() {
    let s = Sandbox::new();
    s.init();
    s.write("top.txt", "t\n");
    s.write("sub/inner.txt", "i\n");
    s.commit_all("one");
    s.write("sub/inner.txt", "edited\n");
    s.write("sub/new.txt", "n\n");
    let sub = std::path::Path::new("sub");
    s.mg_in(sub, &["add", "inner.txt", "../top.txt"]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "M  sub/inner.txt\n?? sub/new.txt");
    s.mg_in(sub, &["add", "."]).ok();
    assert_eq!(s.mg(&["status", "--porcelain"]).out(), "M  sub/inner.txt\nA  sub/new.txt");
    assert!(s.mg_in(sub, &["add", "../.."]).code != 0);
    s.mg_in(sub, &["commit", "-m", "from sub"]).ok();
    assert_eq!(s.mg_in(sub, &["rev-parse", "HEAD"]).out().len(), 40);
}

// ------------------------------------------------------------------ damage and malice

#[test]
fn fsck_reports_damage() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.write("d/b.txt", "b\n");
    s.commit_all("one");
    let ok = s.mg(&["fsck"]).ok();
    assert!(ok.stdout.contains("0 problems"), "{}", ok.stdout);

    // Delete a blob.
    let blob = s.mg(&["hash-object", "a.txt"]).ok().out().to_string();
    let path = s.path().join(".git/objects").join(&blob[..2]).join(&blob[2..]);
    let saved = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let r = s.mg(&["fsck"]);
    assert_eq!(r.code, 1);
    assert!(r.stdout.contains("missing blob") && r.stdout.contains(&blob), "{}", r.stdout);
    // Corrupt it instead.
    let mut bad = saved.clone();
    let n = bad.len();
    bad[n / 2] ^= 0xff;
    std::fs::write(&path, bad).unwrap();
    assert_eq!(s.mg(&["fsck"]).code, 1);
    std::fs::write(&path, &saved).unwrap();
    assert_eq!(s.mg(&["fsck"]).code, 0);
    // A ref to nowhere.
    std::fs::write(s.path().join(".git/refs/heads/broken"), format!("{}\n", "1".repeat(40))).unwrap();
    let r = s.mg(&["fsck"]);
    assert!(r.stdout.contains("refs/heads/broken"), "{}", r.stdout);
    assert_eq!(r.code, 1);
}

fn repo(s: &Sandbox) -> Repo {
    Repo::open(s.path()).unwrap()
}

/// Commits a hand-built tree, so a "malicious" repository can be made without git refusing it.
fn forge_commit(r: &Repo, entries: Vec<TreeEntry>, parent: Option<minivcs::sha1::Oid>) -> minivcs::sha1::Oid {
    let tree = r.odb.write(Kind::Tree, &Tree { entries }.serialize()).unwrap();
    let sig = Signature { name: "Evil".into(), email: "evil@example.com".into(), when: 1_800_000_000, tz: "+0000".into() };
    let c = Commit {
        tree,
        parents: parent.into_iter().collect(),
        author: sig.clone(),
        committer: sig,
        extra: vec![],
        message: "forged\n".into(),
    };
    r.odb.write(Kind::Commit, &c.serialize()).unwrap()
}

#[test]
fn a_tree_with_dangerous_paths_is_never_written_to_disk() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    let good = s.commit_all("good");
    let r = repo(&s);
    let blob = r.odb.write(Kind::Blob, b"PWNED\n").unwrap();
    let dirs_inside = |name: &str| -> Vec<TreeEntry> {
        let inner = r
            .odb
            .write(Kind::Tree, &Tree { entries: vec![TreeEntry { mode: MODE_FILE, name: "x".into(), oid: blob }] }.serialize())
            .unwrap();
        vec![TreeEntry { mode: MODE_DIR, name: name.into(), oid: inner }]
    };
    let hostile: Vec<(&str, Vec<TreeEntry>)> = vec![
        (".git/hooks/x", dirs_inside(".git")),
        (".GIT dir", dirs_inside(".GIT")),
        ("git~1 short name", dirs_inside("git~1")),
        ("dotdot", dirs_inside("..")),
        ("backslash name", vec![TreeEntry { mode: MODE_FILE, name: "..\\evil.txt".into(), oid: blob }]),
        ("drive colon", vec![TreeEntry { mode: MODE_FILE, name: "C:evil.txt".into(), oid: blob }]),
        ("stream", vec![TreeEntry { mode: MODE_FILE, name: "a.txt:stream".into(), oid: blob }]),
        ("trailing dot git", dirs_inside(".git.")),
    ];
    for (what, entries) in hostile {
        let bad = forge_commit(&r, entries, Some(minivcs::sha1::Oid::from_hex(&good).unwrap()));
        s.mg(&["branch", "-D", "evil"]);
        s.mg(&["branch", "evil", &bad.hex()]).ok();
        let out = s.mg(&["checkout", "evil"]);
        assert_ne!(out.code, 0, "{what}: checkout of a hostile tree must fail ({})", out.stdout);
        assert!(out.stderr.contains("unsafe"), "{what}: {}", out.stderr);
        assert!(!s.exists(".git/hooks/x") && !s.exists("../evil.txt") && !s.exists("PWNED"), "{what}: something was written");
        // Still on main, still clean.
        assert_eq!(s.mg(&["status", "--porcelain"]).out(), "", "{what}");
        assert!(s.mg(&["branch"]).stdout.contains("* main"));
    }
    // fsck flags those trees too.
    assert!(s.mg(&["fsck"]).stdout.contains("unsafe entry name"));
}

#[test]
fn a_forged_index_with_a_path_outside_the_work_tree_is_refused() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.commit_all("one");
    let mut idx = minivcs::index::Index::new();
    let blob = repo(&s).odb.write(Kind::Blob, b"x").unwrap();
    // Build the file by hand with the normal serializer, then patch the path to something unsafe.
    idx.add(minivcs::index::Entry::new("aaaa", MODE_FILE, blob));
    let mut bytes = idx.serialize().unwrap();
    let at = bytes.windows(4).position(|w| w == b"aaaa").unwrap();
    bytes[at..at + 4].copy_from_slice(b"../x");
    let body_len = bytes.len() - 20;
    let sum = minivcs::sha1::sha1(&bytes[..body_len]);
    bytes[body_len..].copy_from_slice(&sum.0);
    std::fs::write(s.path().join(".git/index"), bytes).unwrap();
    let r = s.mg(&["status"]);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("unsafe path"), "{}", r.stderr);
}

#[test]
fn a_second_writer_cannot_corrupt_the_index() {
    let s = Sandbox::new();
    s.init();
    s.write("a.txt", "a\n");
    s.commit_all("one");
    std::fs::write(s.path().join(".git/index.lock"), "").unwrap();
    s.write("a.txt", "changed\n");
    let r = s.mg(&["add", "a.txt"]);
    assert_ne!(r.code, 0);
    assert!(r.stderr.contains("index.lock"), "{}", r.stderr);
    std::fs::remove_file(s.path().join(".git/index.lock")).unwrap();
    s.mg(&["add", "a.txt"]).ok();
}
