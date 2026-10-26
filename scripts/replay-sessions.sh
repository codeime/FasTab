#!/usr/bin/env bash
#
# Replay recorded session buffers through `ftab engine complete`.
#
#   scripts/replay-sessions.sh
#   scripts/replay-sessions.sh tests/session-replay/sessions/git.jsonl
#
# Each session is a JSONL of `{buffer, cwd}` where `cwd` is a repo key under
# tests/session-replay/repos/{git,npm,docker,kubectl,cargo}. This recorded the
# T3.2 dual-path diff; with one path left it is a smoke replay, so a completion
# that panics or exits non-zero fails the run.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SESSIONS_DIR="$ROOT/tests/session-replay/sessions"
REPOS_DIR="$ROOT/tests/session-replay/repos"
SPECS_DIR="${EC_SPECS_DIR:-$ROOT/bundle/specs-ir}"

usage() {
    sed -n '3,11p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help) usage 0 ;;
        *) break ;;
    esac
done

if [[ ! -f "$SPECS_DIR/index.json" ]]; then
    echo "missing specs IR at $SPECS_DIR (run node scripts/compile-spec-ir.mjs)" >&2
    exit 2
fi

setup_repos() {
    local git_dir="$REPOS_DIR/git"
    if [[ ! -d "$git_dir/.git" ]]; then
        git init -b main "$git_dir" >/dev/null
        git -C "$git_dir" add README.md
        git -C "$git_dir" -c user.email=session-replay@example.com -c user.name=session-replay \
            commit -m "session replay fixture" >/dev/null
    fi
}

resolve_ec() {
    if [[ -n "${EC_BIN:-}" && -x "$EC_BIN" ]]; then
        printf '%s\n' "$EC_BIN"
        return
    fi
    # `engine` exists only with the engine-cli feature, so an ftab already in
    # target/ may lack it. Building is a no-op when the feature build is current.
    echo "building ftab (cargo build -p fastab_cli --features engine-cli)..." >&2
    cargo build -p fastab_cli --features engine-cli --quiet
    printf '%s\n' "$ROOT/target/debug/ftab"
}

setup_repos
EC="$(resolve_ec)"

if [[ $# -gt 0 ]]; then
    files=("$@")
else
    files=("$SESSIONS_DIR"/*.jsonl)
fi

if [[ ${#files[@]} -eq 0 ]]; then
    echo "no session files" >&2
    exit 2
fi

for file in "${files[@]}"; do
    echo "==> $(basename "$file")"
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -z "${line// }" ]] && continue
        buffer="$(node -e 'const r=JSON.parse(process.argv[1]); process.stdout.write(r.buffer)' "$line")"
        cwd_key="$(node -e 'const r=JSON.parse(process.argv[1]); process.stdout.write(r.cwd)' "$line")"
        case "$cwd_key" in
            git|npm|docker|kubectl|cargo) cwd="$REPOS_DIR/$cwd_key" ;;
            *) cwd="$cwd_key" ;;
        esac
        "$EC" engine complete --buffer "$buffer" --cwd "$cwd" --specs-dir "$SPECS_DIR" >/dev/null
    done < "$file"
done

echo "all sessions completed"
