#!/usr/bin/env bash
# Records two real terminal sessions for the README into target/demo/:
#   session.txt   a short mg session: commit, branch, diverge, merge with a conflict, resolve
#   interop.txt   real git reading and checking the repository mg just wrote
# Needs git on PATH and a release build of mg.
set -uo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
cargo build --release >/dev/null 2>&1
D=$PWD/target/demo
MGBIN=$PWD/target/release/mg.exe
rm -rf "$D" && mkdir -p "$D/bin" "$D/repo"
cp "$MGBIN" "$D/bin/mg.exe"
export PATH="$D/bin:$PATH"
export GIT_AUTHOR_NAME="Ada Lovelace" GIT_AUTHOR_EMAIL="ada@example.com" GIT_COMMITTER_NAME="Ada Lovelace" GIT_COMMITTER_EMAIL="ada@example.com"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
t=1730000000
tick() { t=$((t + 3600)); export GIT_AUTHOR_DATE="$t +0000" GIT_COMMITTER_DATE="$t +0000"; }
cd "$D/repo"
run() {
  tick
  local shown="\$"
  for a in "$@"; do
    case "$a" in *" "*) shown="$shown \"$a\"" ;; *) shown="$shown $a" ;; esac
  done
  echo "$shown"
  "$@" 2>&1 | sed 's/\r$//'
}

{
  run mg init
  printf 'fn main() {\n    println!("hello");\n}\n' > main.rs
  printf '# demo\n' > README.md
  run mg add .
  run mg commit -m "first commit"
  run mg checkout -b greeting
  printf 'fn main() {\n    println!("hello, world");\n}\n' > main.rs
  run mg commit -a -m "friendlier greeting"
  run mg checkout main
  printf 'fn main() {\n    println!("hi");\n}\n' > main.rs
  run mg commit -a -m "shorter greeting"
  run mg merge greeting
  run mg status -s
  cat main.rs
  printf 'fn main() {\n    println!("hello, world!");\n}\n' > main.rs
  echo
  run mg add main.rs
  run mg commit -m "merge greeting"
  run mg log --oneline
} > "$D/session.txt"

{
  run git fsck --strict
  run git log --oneline --graph
  run git status
  run git cat-file -p HEAD
} > "$D/interop.txt"
cat "$D/session.txt" "$D/interop.txt"
