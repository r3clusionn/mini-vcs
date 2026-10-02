use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use minivcs::error::Result;
use minivcs::history::{MergeOutcome, ResetMode};
use minivcs::object::{Kind, Tree, MODE_DIR};
use minivcs::repo::{Head, Repo};
use minivcs::sha1::Oid;
use minivcs::show::{format_commit, format_custom};
use minivcs::worktree::normalize_pathspec;
use minivcs::{err, show};

/// A git-compatible version control system.
#[derive(Parser)]
#[command(name = "mg", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a repository
    Init {
        dir: Option<PathBuf>,
        /// Name of the first branch
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Stage files; a path that no longer exists is staged as deleted
    Add {
        /// Also add files that .gitignore ignores
        #[arg(short, long)]
        force: bool,
        paths: Vec<String>,
    },
    /// Stop tracking files (and delete them unless --cached)
    Rm {
        #[arg(long)]
        cached: bool,
        #[arg(short, long)]
        force: bool,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Show staged, unstaged and untracked changes
    Status {
        /// Short format (two columns of status letters, like git status -s)
        #[arg(short, long)]
        short: bool,
        /// Stable machine-readable format (same as --short without hints)
        #[arg(long)]
        porcelain: bool,
    },
    /// Record the staged changes
    Commit {
        #[arg(short, long)]
        message: Vec<String>,
        /// Stage every change to tracked files first
        #[arg(short, long)]
        all: bool,
        /// Replace the last commit
        #[arg(long)]
        amend: bool,
        #[arg(long)]
        allow_empty: bool,
    },
    /// Show history
    Log {
        revs: Vec<String>,
        #[arg(short = 'n', long)]
        max_count: Option<usize>,
        #[arg(long)]
        oneline: bool,
        /// Custom format: %H %h %T %P %p %an %ae %ad %at %cn %ce %ct %s %b %n %%
        #[arg(long)]
        format: Option<String>,
    },
    /// Show a commit (or any object) with its changes
    Show { rev: Option<String> },
    /// Show changes: work tree vs index, or with --cached index vs HEAD, or between two commits
    Diff {
        #[arg(long, alias = "staged")]
        cached: bool,
        #[arg(long)]
        name_status: bool,
        #[arg(long)]
        name_only: bool,
        /// Zero, one or two commits
        revs: Vec<String>,
        /// After `--`: limit the diff to these paths
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// List, create, delete or rename branches
    Branch {
        name: Option<String>,
        start: Option<String>,
        #[arg(short, long)]
        delete: bool,
        #[arg(short = 'D')]
        force_delete: bool,
        #[arg(short = 'm', long = "move")]
        rename: bool,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Switch branches or commits
    Checkout {
        /// Create a branch and switch to it
        #[arg(short = 'b')]
        new_branch: Option<String>,
        /// Throw away local changes that are in the way
        #[arg(short, long)]
        force: bool,
        /// A branch, tag or commit to switch to (or, before `--`, where to restore paths from)
        target: Option<String>,
        /// After `--`: paths to restore from the index (or from the target commit)
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// Restore files from the index (or --source), or unstage with --staged
    Restore {
        #[arg(long)]
        staged: bool,
        #[arg(long)]
        source: Option<String>,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Move the branch to another commit, optionally resetting the index and files
    Reset {
        #[arg(long)]
        soft: bool,
        #[arg(long)]
        mixed: bool,
        #[arg(long)]
        hard: bool,
        /// The commit to move to (default HEAD)
        target: Option<String>,
        /// After `--`: paths to unstage instead of moving the branch
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// List, create or delete tags
    Tag {
        name: Option<String>,
        rev: Option<String>,
        /// Make an annotated tag (needs -m)
        #[arg(short = 'a', long)]
        annotate: bool,
        /// The tag message; implies an annotated tag
        #[arg(short, long)]
        message: Option<String>,
        #[arg(short, long)]
        delete: bool,
    },
    /// Join another branch into the current one
    Merge {
        branch: Option<String>,
        #[arg(long)]
        no_ff: bool,
        #[arg(long)]
        abort: bool,
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Show the type, size or content of an object
    CatFile {
        #[arg(short = 't')]
        kind: bool,
        #[arg(short = 's')]
        size: bool,
        #[arg(short = 'p')]
        pretty: bool,
        object: String,
    },
    /// Compute an object id, optionally storing the object
    HashObject {
        #[arg(short = 'w')]
        write: bool,
        #[arg(long)]
        stdin: bool,
        files: Vec<PathBuf>,
    },
    /// List the contents of a tree
    LsTree {
        #[arg(short, long)]
        recursive: bool,
        rev: String,
    },
    /// List the files in the index
    LsFiles {
        #[arg(short, long)]
        stage: bool,
    },
    /// Resolve a revision to an object id
    RevParse { rev: String },
    /// Write the index as a tree object and print its id
    WriteTree,
    /// Verify every object and ref
    Fsck,
    /// Read or write a repository config value
    Config { key: String, value: Option<String> },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("mg: {e}");
            ExitCode::from(1)
        }
    }
}

fn open() -> Result<(Repo, String)> {
    let cwd = std::env::current_dir()?;
    let repo = Repo::discover(&cwd)?;
    let here = std::fs::canonicalize(&cwd)?;
    let here = PathBuf::from(here.to_string_lossy().trim_start_matches(r"\\?\"));
    let rel = here.strip_prefix(&repo.work).unwrap_or(Path::new("")).to_string_lossy().replace('\\', "/");
    Ok((repo, rel))
}

fn specs(cwd: &str, paths: &[String]) -> Result<Vec<String>> {
    paths.iter().map(|p| normalize_pathspec(cwd, p)).collect()
}

fn rev(repo: &Repo, s: &str) -> Result<Oid> {
    repo.rev_parse(s)
}

fn short_head(repo: &Repo) -> Result<String> {
    Ok(match repo.head()? {
        Head::Branch(b) => b,
        Head::Detached(o) => format!("HEAD detached at {}", o.short()),
    })
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Cmd::Init { dir, branch } => {
            let dir = dir.unwrap_or_else(|| PathBuf::from("."));
            std::fs::create_dir_all(&dir)?;
            match Repo::init(&dir, &branch)? {
                Some(_) => println!(
                    "Initialized an empty repository in {}",
                    dir.join(".git").display().to_string().trim_start_matches("./").trim_start_matches(".\\")
                ),
                None => println!("Repository already exists in {}", dir.join(".git").display()),
            }
        }
        Cmd::Add { force, paths } => {
            let (repo, cwd) = open()?;
            if paths.is_empty() {
                return Err(err!("nothing specified, nothing added (try: mg add .)"));
            }
            let mut index = repo.read_index()?;
            let report = repo.add_paths(&mut index, &specs(&cwd, &paths)?, force)?;
            repo.write_index(&index)?;
            for p in &report.ignored {
                eprintln!("mg: {p} is ignored by a .gitignore file (use -f to add it anyway)");
            }
            for p in &report.skipped {
                eprintln!("mg: not adding {p}");
            }
            if !report.ignored.is_empty() && report.added.is_empty() && report.removed.is_empty() {
                return Ok(ExitCode::from(1));
            }
        }
        Cmd::Rm { cached, force, paths } => {
            let (repo, cwd) = open()?;
            let mut index = repo.read_index()?;
            for spec in specs(&cwd, &paths)? {
                let prefix = format!("{spec}/");
                let doomed: Vec<_> = index.entries().filter(|e| e.path == spec || e.path.starts_with(&prefix)).cloned().collect();
                if doomed.is_empty() {
                    return Err(err!("pathspec {spec:?} did not match any tracked file"));
                }
                for e in doomed {
                    if !cached {
                        let abs = repo.work.join(&e.path);
                        if abs.is_file() && !force {
                            let data = std::fs::read(&abs)?;
                            if minivcs::object::object_id(Kind::Blob, &data) != e.oid {
                                return Err(err!(
                                    "{} has local modifications (use -f to delete it anyway, or --cached to keep the file)",
                                    e.path
                                ));
                            }
                        }
                        let _ = std::fs::remove_file(&abs);
                        let mut dir = abs.parent().map(Path::to_path_buf);
                        while let Some(d) = dir {
                            if d == repo.work || std::fs::remove_dir(&d).is_err() {
                                break;
                            }
                            dir = d.parent().map(Path::to_path_buf);
                        }
                    }
                    index.remove(&e.path);
                    println!("rm '{}'", e.path);
                }
            }
            repo.write_index(&index)?;
        }
        Cmd::Status { short, porcelain } => {
            let (repo, _) = open()?;
            let st = repo.status()?;
            if short || porcelain {
                print!("{}", st.porcelain());
            } else {
                print_status(&repo, &st)?;
            }
        }
        Cmd::Commit { message, all, amend, allow_empty } => {
            let (repo, _) = open()?;
            if all {
                let mut index = repo.read_index()?;
                let tracked: Vec<String> = index.entries().filter(|e| e.stage == 0).map(|e| e.path.clone()).collect();
                for p in tracked {
                    if repo.work.join(&p).is_file() {
                        repo.add_paths(&mut index, std::slice::from_ref(&p), true)?;
                    } else {
                        index.remove(&p);
                    }
                }
                repo.write_index(&index)?;
            }
            let msg = if message.is_empty() {
                match std::fs::read_to_string(repo.git.join("MERGE_MSG")) {
                    Ok(m) => m,
                    Err(_) if amend => String::new(),
                    Err(_) => return Err(err!("a commit message is required: use -m \"message\"")),
                }
            } else {
                message.join("\n\n")
            };
            let before = repo.head_commit()?;
            let id = repo.commit(&msg, allow_empty, amend)?;
            let c = repo.read_commit(&id)?;
            let branch = match repo.head()? {
                Head::Branch(b) => b,
                Head::Detached(_) => "detached HEAD".into(),
            };
            let root = if before.is_none() { " (root-commit)" } else { "" };
            println!("[{branch}{root} {}] {}", id.short(), c.subject());
        }
        Cmd::Log { revs, max_count, oneline, format } => {
            let (repo, _) = open()?;
            let starts: Vec<Oid> = if revs.is_empty() {
                match repo.head_commit()? {
                    Some(h) => vec![h],
                    None => return Err(err!("your current branch has no commits yet")),
                }
            } else {
                revs.iter().map(|r| rev(&repo, r)).collect::<Result<_>>()?
            };
            let mut out = std::io::stdout().lock();
            let commits = repo.log(&starts, max_count)?;
            for (i, (id, c)) in commits.iter().enumerate() {
                if let Some(f) = &format {
                    writeln!(out, "{}", format_custom(f, id, c)).ok();
                } else if oneline {
                    writeln!(out, "{} {}", id.short(), c.subject()).ok();
                } else {
                    if i > 0 {
                        writeln!(out).ok();
                    }
                    write!(out, "{}", format_commit(id, c)).ok();
                }
            }
        }
        Cmd::Show { rev: spec } => {
            let (repo, _) = open()?;
            let id = rev(&repo, spec.as_deref().unwrap_or("HEAD"))?;
            let (kind, data) = repo.odb.read(&id)?;
            match kind {
                Kind::Commit => {
                    let c = repo.read_commit(&id)?;
                    print!("{}", format_commit(&id, &c));
                    if c.parents.len() <= 1 {
                        let changes = repo.diff_commits(c.parents.first(), Some(&id))?;
                        let diff = repo.render_diff(&changes)?;
                        if !diff.is_empty() {
                            println!();
                            print!("{diff}");
                        }
                    }
                }
                Kind::Tag => {
                    let t = minivcs::object::TagObject::parse(&data)?;
                    println!("tag {}", t.name);
                    if let Some(tg) = &t.tagger {
                        println!("Tagger: {} <{}>\nDate:   {}", tg.name, tg.email, show::format_date(tg.when, &tg.tz));
                    }
                    println!("\n{}", t.message.trim_end());
                    println!();
                    let target = repo.peel_to_commit(&id)?;
                    print!("{}", format_commit(&target, &repo.read_commit(&target)?));
                }
                Kind::Tree => {
                    println!("tree {id}\n");
                    for e in Tree::parse(&data)?.entries {
                        println!("{}{}", e.name, if e.mode == MODE_DIR { "/" } else { "" });
                    }
                }
                Kind::Blob => {
                    std::io::stdout().write_all(&data).ok();
                }
            }
        }
        Cmd::Diff { cached, name_status, name_only, revs, paths } => {
            let (repo, cwd) = open()?;
            let mut changes = match revs.len() {
                0 if cached => repo.diff_cached()?,
                0 => repo.diff_work()?,
                1 if cached => {
                    // Index against a given commit.
                    let c = rev(&repo, &revs[0])?;
                    let index = repo.read_index()?;
                    let staged = index.entries().filter(|e| e.stage == 0).map(|e| (e.path.clone(), (e.mode, e.oid))).collect();
                    show::changes(&repo.commit_files(Some(&repo.peel_to_commit(&c)?))?, &staged)
                }
                1 => {
                    return Err(err!("give two revisions to compare, or none for the work tree"));
                }
                2 => repo.diff_commits(
                    Some(&repo.peel_to_commit(&rev(&repo, &revs[0])?)?),
                    Some(&repo.peel_to_commit(&rev(&repo, &revs[1])?)?),
                )?,
                _ => return Err(err!("too many revisions")),
            };
            if !paths.is_empty() {
                let paths = specs(&cwd, &paths)?;
                changes.retain(|c| paths.iter().any(|p| p.is_empty() || c.path == *p || c.path.starts_with(&format!("{p}/"))));
            }
            if name_status {
                for c in &changes {
                    println!("{}\t{}", c.status_letter(), c.path);
                }
            } else if name_only {
                for c in &changes {
                    println!("{}", c.path);
                }
            } else {
                print!("{}", repo.render_diff(&changes)?);
            }
        }
        Cmd::Branch { name, start, delete, force_delete, rename, verbose } => {
            let (repo, _) = open()?;
            match (name, delete || force_delete, rename) {
                (None, _, _) => {
                    let current = repo.head()?;
                    for (b, tip) in repo.branches()? {
                        let mark = if current == Head::Branch(b.clone()) { "*" } else { " " };
                        if verbose {
                            println!("{mark} {b} {} {}", tip.short(), repo.read_commit(&tip)?.subject());
                        } else {
                            println!("{mark} {b}");
                        }
                    }
                    if let (Head::Branch(b), None) = (&current, repo.head_commit()?) {
                        println!("* {b}  (no commits yet)");
                    }
                }
                (Some(n), true, _) => {
                    let tip = repo.delete_branch(&n, force_delete)?;
                    println!("Deleted branch {n} (was {}).", tip.short());
                }
                (Some(old), _, true) => {
                    let new = start.ok_or_else(|| err!("give the new name: mg branch -m old new"))?;
                    repo.rename_branch(&old, &new)?;
                }
                (Some(n), false, false) => {
                    let at = match start {
                        Some(s) => rev(&repo, &s)?,
                        None => repo.head_commit()?.ok_or_else(|| err!("cannot create a branch before the first commit"))?,
                    };
                    repo.create_branch(&n, &at)?;
                }
            }
        }
        Cmd::Checkout { new_branch, force, target, paths } => {
            let (repo, cwd) = open()?;
            if !paths.is_empty() {
                let paths = specs(&cwd, &paths)?;
                let source = match &target {
                    Some(r) => Some(repo.peel_to_commit(&rev(&repo, r)?)?),
                    None => None,
                };
                let n = repo.restore_files(&paths, source.as_ref())?;
                println!(
                    "Updated {n} path{} from {}",
                    if n == 1 { "" } else { "s" },
                    if source.is_some() { "the commit" } else { "the index" }
                );
                return Ok(ExitCode::SUCCESS);
            }
            if let Some(nb) = new_branch {
                let at = match &target {
                    Some(t) => repo.peel_to_commit(&rev(&repo, t)?)?,
                    None => repo.head_commit()?.ok_or_else(|| err!("cannot create a branch before the first commit"))?,
                };
                repo.create_branch(&nb, &at)?;
                if target.is_some() {
                    if let Err(e) = repo.switch_to(Some(&at), force) {
                        let _ = repo.delete_ref(&format!("refs/heads/{nb}"));
                        return Err(e);
                    }
                }
                repo.set_head_branch(&nb)?;
                println!("Switched to a new branch '{nb}'");
                return Ok(ExitCode::SUCCESS);
            }
            let target = target.ok_or_else(|| err!("say what to check out: a branch, a tag or a commit"))?;
            if repo.read_ref(&format!("refs/heads/{target}"))?.is_some() {
                let tip = repo.read_ref(&format!("refs/heads/{target}"))?.unwrap();
                repo.switch_to(Some(&tip), force)?;
                repo.set_head_branch(&target)?;
                println!("Switched to branch '{target}'");
            } else {
                let c = repo.peel_to_commit(&rev(&repo, &target)?)?;
                repo.switch_to(Some(&c), force)?;
                repo.set_head_detached(&c)?;
                println!("HEAD is now at {} {}", c.short(), repo.read_commit(&c)?.subject());
            }
            repo.clear_merge_state()?;
        }
        Cmd::Restore { staged, source, paths } => {
            let (repo, cwd) = open()?;
            let paths = specs(&cwd, &paths)?;
            if staged {
                if let Some(s) = source {
                    return Err(err!("--staged with --source {s} is not supported"));
                }
                repo.unstage(&paths)?;
            } else {
                let src = match source {
                    Some(s) => Some(repo.peel_to_commit(&rev(&repo, &s)?)?),
                    None => None,
                };
                repo.restore_files(&paths, src.as_ref())?;
            }
        }
        Cmd::Reset { soft, mixed, hard, target, paths } => {
            let (repo, cwd) = open()?;
            if !paths.is_empty() {
                repo.unstage(&specs(&cwd, &paths)?)?;
                return Ok(ExitCode::SUCCESS);
            }
            if [soft, mixed, hard].iter().filter(|b| **b).count() > 1 {
                return Err(err!("choose one of --soft, --mixed, --hard"));
            }
            let mode = if soft {
                ResetMode::Soft
            } else if hard {
                ResetMode::Hard
            } else {
                ResetMode::Mixed
            };
            let target = rev(&repo, target.as_deref().unwrap_or("HEAD"))?;
            repo.reset(&target, mode)?;
            if hard {
                println!("HEAD is now at {} {}", target.short(), repo.read_commit(&repo.peel_to_commit(&target)?)?.subject());
            }
        }
        Cmd::Tag { name, rev: spec, annotate, message, delete } => {
            let (repo, _) = open()?;
            match name {
                None => {
                    for (t, _) in repo.tags()? {
                        println!("{t}");
                    }
                }
                Some(n) if delete => {
                    repo.delete_tag(&n)?;
                    println!("Deleted tag '{n}'");
                }
                Some(n) => {
                    let at = match spec {
                        Some(s) => rev(&repo, &s)?,
                        None => repo.head_commit()?.ok_or_else(|| err!("cannot tag before the first commit"))?,
                    };
                    if annotate && message.is_none() {
                        return Err(err!("an annotated tag needs a message: -m \"text\""));
                    }
                    repo.create_tag(&n, &at, message.as_deref())?;
                }
            }
        }
        Cmd::Merge { branch, no_ff, abort, message } => {
            let (repo, _) = open()?;
            if abort {
                repo.merge_abort()?;
                println!("Merge aborted.");
                return Ok(ExitCode::SUCCESS);
            }
            let branch = branch.ok_or_else(|| err!("say what to merge: mg merge <branch>"))?;
            match repo.merge(&branch, no_ff, message.as_deref())? {
                MergeOutcome::AlreadyUpToDate => println!("Already up to date."),
                MergeOutcome::FastForward(id) => println!("Fast-forward to {}", id.short()),
                MergeOutcome::Merged(id) => println!("Merge made commit {}.", id.short()),
                MergeOutcome::Conflicts(paths) => {
                    for p in &paths {
                        println!("CONFLICT: {p}");
                    }
                    println!("Automatic merge failed; fix the conflicts, mg add the files, then mg commit.");
                    return Ok(ExitCode::from(1));
                }
            }
        }
        Cmd::CatFile { kind, size, pretty, object } => {
            let (repo, _) = open()?;
            let id = rev(&repo, &object)?;
            let (k, data) = repo.odb.read(&id)?;
            if kind {
                println!("{}", k.name());
            } else if size {
                println!("{}", data.len());
            } else if pretty {
                match k {
                    Kind::Tree => {
                        for e in Tree::parse(&data)?.entries {
                            let t = if e.mode == MODE_DIR { "tree" } else { "blob" };
                            println!("{:06o} {t} {}\t{}", e.mode, e.oid, e.name);
                        }
                    }
                    _ => {
                        std::io::stdout().write_all(&data).ok();
                    }
                }
            } else {
                return Err(err!("give one of -t, -s or -p"));
            }
        }
        Cmd::HashObject { write, stdin, files } => {
            let repo = if write { Some(open()?.0) } else { None };
            let mut inputs: Vec<Vec<u8>> = Vec::new();
            if stdin {
                let mut b = Vec::new();
                std::io::stdin().read_to_end(&mut b)?;
                inputs.push(b);
            }
            for f in files {
                inputs.push(std::fs::read(&f).map_err(|e| err!("{}: {e}", f.display()))?);
            }
            for data in inputs {
                let id = match &repo {
                    Some(r) => r.odb.write(Kind::Blob, &data)?,
                    None => minivcs::object::object_id(Kind::Blob, &data),
                };
                println!("{id}");
            }
        }
        Cmd::LsTree { recursive, rev: spec } => {
            let (repo, _) = open()?;
            let id = repo.peel(&rev(&repo, &spec)?)?;
            let tree_id = match repo.odb.read(&id)?.0 {
                Kind::Commit => repo.read_commit(&id)?.tree,
                _ => id,
            };
            if recursive {
                for (p, (mode, oid)) in repo.tree_files(&tree_id)? {
                    println!("{mode:06o} blob {oid}\t{p}");
                }
            } else {
                for e in repo.read_tree(&tree_id)?.entries {
                    let t = if e.mode == MODE_DIR { "tree" } else { "blob" };
                    println!("{:06o} {t} {}\t{}", e.mode, e.oid, e.name);
                }
            }
        }
        Cmd::LsFiles { stage } => {
            let (repo, _) = open()?;
            for e in repo.read_index()?.entries() {
                if stage {
                    println!("{:06o} {} {}\t{}", e.mode, e.oid, e.stage, e.path);
                } else {
                    println!("{}", e.path);
                }
            }
        }
        Cmd::RevParse { rev: spec } => {
            let (repo, _) = open()?;
            println!("{}", rev(&repo, &spec)?);
        }
        Cmd::WriteTree => {
            let (repo, _) = open()?;
            println!("{}", repo.write_tree(&repo.read_index()?)?);
        }
        Cmd::Fsck => {
            let (repo, _) = open()?;
            let r = repo.fsck()?;
            for p in &r.problems {
                println!("error: {p}");
            }
            for d in &r.dangling {
                println!("dangling {d}");
            }
            println!(
                "checked {} objects, {} problem{}",
                r.objects,
                r.problems.len(),
                if r.problems.len() == 1 { "" } else { "s" }
            );
            if !r.problems.is_empty() {
                return Ok(ExitCode::from(1));
            }
        }
        Cmd::Config { key, value } => {
            let (repo, _) = open()?;
            match value {
                Some(v) => repo.set_config(&key, &v)?,
                None => match repo.config_get(&key) {
                    Some(v) => println!("{v}"),
                    None => return Ok(ExitCode::from(1)),
                },
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_status(repo: &Repo, st: &minivcs::worktree::Status) -> Result<()> {
    println!("On branch {}", short_head(repo)?);
    if repo.head_commit()?.is_none() {
        println!("\nNo commits yet");
    }
    let word = |c: char| match c {
        'A' => "new file:   ",
        'M' => "modified:   ",
        'D' => "deleted:    ",
        _ => "changed:    ",
    };
    let unmerged: Vec<_> =
        st.entries.iter().filter(|e| matches!((e.x, e.y), ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D'))).collect();
    let staged: Vec<_> = st.entries.iter().filter(|e| e.x != ' ' && !unmerged.contains(e)).collect();
    let unstaged: Vec<_> = st.entries.iter().filter(|e| e.y != ' ' && !unmerged.contains(e)).collect();
    if !unmerged.is_empty() {
        println!("\nUnmerged paths:\n  (fix the conflicts, then \"mg add <file>...\")\n");
        for e in unmerged {
            let w = match (e.x, e.y) {
                ('U', 'U') => "both modified:   ",
                ('A', 'A') => "both added:      ",
                ('D', 'D') => "both deleted:    ",
                ('A', 'U') => "added by us:     ",
                ('U', 'A') => "added by them:   ",
                ('D', 'U') => "deleted by us:   ",
                _ => "deleted by them: ",
            };
            println!("\t{w}{}", e.path);
        }
    }
    if !staged.is_empty() {
        println!("\nChanges to be committed:\n  (use \"mg restore --staged <file>...\" to unstage)\n");
        for e in staged {
            println!("\t{}{}", word(e.x), e.path);
        }
    }
    if !unstaged.is_empty() {
        println!("\nChanges not staged for commit:\n  (use \"mg add <file>...\" to update what will be committed)\n");
        for e in unstaged {
            println!("\t{}{}", word(e.y), e.path);
        }
    }
    if !st.untracked.is_empty() {
        println!("\nUntracked files:\n  (use \"mg add <file>...\" to include in what will be committed)\n");
        for u in &st.untracked {
            println!("\t{u}");
        }
    }
    if st.entries.is_empty() {
        println!(
            "\n{}",
            if st.untracked.is_empty() {
                "nothing to commit, working tree clean"
            } else {
                "nothing added to commit but untracked files present"
            }
        );
    }
    Ok(())
}
