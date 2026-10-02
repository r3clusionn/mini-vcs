//! "Racily clean" files: a file changed right after it was staged, with the same size, in the same
//! clock tick, has the same stat data as the index remembers. Trusting the stat data would hide
//! the change, so entries as new as the index itself are always compared by content.
//!
//! The commands of `mg` take milliseconds each, so the window is only reachable in-process.

use minivcs::repo::Repo;

#[test]
fn a_change_right_after_add_is_never_missed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::init(dir.path(), "main").unwrap().unwrap();
    let file = dir.path().join("f.txt");
    let mut missed = Vec::new();
    for i in 0..500u32 {
        // Same length, different content, written back to back.
        std::fs::write(&file, format!("version A {i:06}\n")).unwrap();
        let mut index = repo.read_index().unwrap();
        repo.add_paths(&mut index, &["f.txt".to_string()], true).unwrap();
        repo.write_index(&index).unwrap();
        std::fs::write(&file, format!("version B {i:06}\n")).unwrap();
        let st = repo.status().unwrap();
        let line = st.porcelain();
        if !line.starts_with("AM f.txt") {
            missed.push((i, line));
        }
        // Start over for the next round.
        let mut index = repo.read_index().unwrap();
        index.clear();
        repo.write_index(&index).unwrap();
    }
    assert!(missed.is_empty(), "{} of 500 changes were missed, first: {:?}", missed.len(), missed.first());
}
