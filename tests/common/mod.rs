#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Out {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Out {
    pub fn ok(self) -> Out {
        assert_eq!(self.code, 0, "command failed: {}\n{}", self.stdout, self.stderr);
        self
    }

    pub fn out(&self) -> &str {
        self.stdout.trim_end()
    }
}

/// A scratch directory with helpers to run `mg` and `git` in it.
pub struct Sandbox {
    pub dir: tempfile::TempDir,
    clock: AtomicU64,
}

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn mg_exe() -> &'static str {
    env!("CARGO_BIN_EXE_mg")
}

impl Sandbox {
    pub fn new() -> Sandbox {
        Sandbox { dir: tempfile::tempdir().unwrap(), clock: AtomicU64::new(1_700_000_000) }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A fresh timestamp: every commit gets its own, so hashes are deterministic and ordered.
    pub fn tick(&self) -> String {
        format!("{} +0000", self.clock.fetch_add(60, Ordering::SeqCst))
    }

    fn base(cmd: &mut Command, dir: &Path, date: &str) {
        cmd.current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
    }

    fn run(mut cmd: Command) -> Out {
        let o = cmd.stdin(Stdio::null()).output().expect("run command");
        Out {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            code: o.status.code().unwrap_or(-1),
        }
    }

    pub fn mg(&self, args: &[&str]) -> Out {
        self.mg_in(Path::new(""), args)
    }

    pub fn mg_in(&self, sub: &Path, args: &[&str]) -> Out {
        let date = self.tick();
        self.mg_dated(sub, &date, args)
    }

    pub fn mg_dated(&self, sub: &Path, date: &str, args: &[&str]) -> Out {
        let mut c = Command::new(mg_exe());
        c.args(args);
        Self::base(&mut c, &self.path().join(sub), date);
        Self::run(c)
    }

    pub fn git(&self, args: &[&str]) -> Out {
        let date = self.tick();
        self.git_dated(&date, args)
    }

    pub fn git_dated(&self, date: &str, args: &[&str]) -> Out {
        let mut c = Command::new("git");
        c.args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.quotepath=false",
            "-c",
            "color.ui=false",
            "-c",
            "diff.renames=false",
            "-c",
            "core.safecrlf=false",
            "-c",
            "advice.detachedHead=false",
        ]);
        c.args(args);
        Self::base(&mut c, self.path(), date);
        Self::run(c)
    }

    pub fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path().join(rel)).unwrap()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path().join(rel).exists()
    }

    pub fn remove(&self, rel: &str) {
        let p: PathBuf = self.path().join(rel);
        if p.is_dir() {
            std::fs::remove_dir_all(p).unwrap();
        } else {
            std::fs::remove_file(p).unwrap();
        }
    }

    /// `mg init`, with identity taken from the environment variables set above.
    pub fn init(&self) {
        self.mg(&["init"]).ok();
    }

    pub fn commit_all(&self, msg: &str) -> String {
        self.mg(&["add", "."]).ok();
        self.mg(&["commit", "-m", msg]).ok();
        self.mg(&["rev-parse", "HEAD"]).ok().out().to_string()
    }
}
