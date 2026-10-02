"""Times mg against git on the same work: initial add and commit, status, diff, checkout and log.

    python scripts/bench.py [FILE_COUNT] [CORPUS_DIR]

The corpus is the first FILE_COUNT files (sorted by path) of a directory of real source code,
default the Rust crate sources in ~/.cargo/registry/src. Each step is run 5 times and the median
is reported. Needs git on PATH and a release build of mg.
"""
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

MG = Path(__file__).resolve().parent.parent / "target" / "release" / ("mg.exe" if os.name == "nt" else "mg")
GIT_FLAGS = ["-c", "core.autocrlf=false", "-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false", "-c", "gc.auto=0"]
ENV = dict(
    os.environ,
    GIT_AUTHOR_NAME="Bench", GIT_AUTHOR_EMAIL="b@example.com", GIT_COMMITTER_NAME="Bench", GIT_COMMITTER_EMAIL="b@example.com",
    GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
)


def run(tool, args, cwd, check=True):
    cmd = ["git", *GIT_FLAGS, *args] if tool == "git" else [str(MG), *args]
    t = time.perf_counter()
    r = subprocess.run(cmd, cwd=cwd, env=ENV, capture_output=True)
    dt = time.perf_counter() - t
    if check and r.returncode not in (0,):
        raise SystemExit(f"{tool} {args} failed: {r.stderr.decode(errors='replace')[:300]}")
    return dt, r.stdout


def corpus(count, src):
    files = []
    for root, dirs, names in os.walk(src):
        dirs.sort()
        for n in sorted(names):
            p = Path(root) / n
            try:
                if p.stat().st_size <= 200_000:
                    files.append(p)
            except OSError:
                pass
            if len(files) >= count:
                return files, Path(src)
    return files, Path(src)


def populate(dest, files, base):
    for f in files:
        rel = f.relative_to(base)
        t = dest / rel
        t.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(f, t)


def median(f, runs=5):
    return statistics.median(f() for _ in range(runs))


def main():
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
    src = sys.argv[2] if len(sys.argv) > 2 else str(Path.home() / ".cargo" / "registry" / "src")
    files, base = corpus(count, src)
    total = sum(f.stat().st_size for f in files)
    print(f"corpus: {len(files)} files, {total / 1e6:.1f} MB, from {len(files) and files[0].parts[-4]}... (real crate sources)")
    work = Path(tempfile.mkdtemp(prefix="mgbench-"))
    rows = []
    try:
        results = {}
        for tool in ("git", "mg"):
            r = {}
            d = work / tool
            d.mkdir()
            populate(d, files, base)
            run(tool, ["init", "-q", "-b", "main"] if tool == "git" else ["init"], d)
            r["add"], _ = run(tool, ["add", "."], d)
            r["commit"], _ = run(tool, ["commit", "-q", "-m", "initial"] if tool == "git" else ["commit", "-m", "initial"], d)
            r["status (clean)"] = median(lambda: run(tool, ["status", "--porcelain"], d)[0])
            # Change 200 files.
            changed = files[:: max(1, len(files) // 200)][:200]
            for f in changed:
                with open(d / f.relative_to(base), "ab") as fh:
                    fh.write(b"\n// benchmark edit\n")
            r["status (200 modified)"] = median(lambda: run(tool, ["status", "--porcelain"], d)[0])
            r["diff (200 modified)"] = median(lambda: run(tool, ["diff"], d)[0])
            r["add (200 modified)"], _ = run(tool, ["add", "."], d)
            r["commit (200 modified)"], _ = run(tool, ["commit", "-q", "-m", "edit"] if tool == "git" else ["commit", "-m", "edit"], d)
            # Switch between two commits that differ in 200 files.
            def switch():
                a = run(tool, ["checkout", "-q", "HEAD~1"] if tool == "git" else ["checkout", "HEAD~1"], d)[0]
                b = run(tool, ["checkout", "-q", "main"] if tool == "git" else ["checkout", "main"], d)[0]
                return (a + b) / 2
            r["checkout (200 files change)"] = median(switch, 3)
            r["log --oneline (2 commits)"] = median(lambda: run(tool, ["log", "--oneline"], d)[0])
            results[tool] = r
        # History: 3000 commits built with fast-import, read by both.
        h = work / "history"
        h.mkdir()
        run("git", ["init", "-q", "-b", "main"], h)
        stream = []
        for i in range(3000):
            msg = f"commit number {i}\n".encode()
            stream.append(f"commit refs/heads/main\ncommitter B <b@example.com> {1700000000 + i * 60} +0000\ndata {len(msg)}\n".encode() + msg)
            stream.append(f"M 100644 inline f{i % 50}.txt\ndata 8\nv{i:06}\n\n".encode())
        subprocess.run(["git", *GIT_FLAGS, "fast-import", "--quiet"], cwd=h, env=ENV, input=b"".join(stream), check=True)
        # fast-import writes a packfile, which mg does not read; unpack it into loose objects.
        packdir = h / ".git" / "objects" / "pack"
        for pack in packdir.glob("*.pack"):
            data = pack.read_bytes()
            for victim in [pack, *packdir.glob("*.idx")]:
                os.chmod(victim, 0o666)  # git makes its files read-only
                victim.unlink()
            subprocess.run(["git", *GIT_FLAGS, "unpack-objects", "-q"], cwd=h, env=ENV, input=data, check=True)
        out_g = run("git", ["log", "--oneline"], h)[1]
        out_m = run("mg", ["log", "--oneline"], h)[1]
        same = out_g == out_m
        results["git"]["log --oneline (3000 commits)"] = median(lambda: run("git", ["log", "--oneline"], h)[0])
        results["mg"]["log --oneline (3000 commits)"] = median(lambda: run("mg", ["log", "--oneline"], h)[0])
        print(f"log of 3000 commits identical to git's: {same}")

        print(f"\n{'step':<34}{'git':>10}{'mg':>10}")
        for k in results["git"]:
            print(f"{k:<34}{results['git'][k] * 1000:>8.0f}ms{results['mg'][k] * 1000:>8.0f}ms")
    finally:
        shutil.rmtree(work, onerror=lambda f, p, _: (os.chmod(p, 0o666), f(p)))


if __name__ == "__main__":
    main()
