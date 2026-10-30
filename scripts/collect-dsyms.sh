#!/bin/bash
set -euo pipefail

# ── Pair each stripped binary with its dSYM ───────────────────────────
#
# The dist profile writes line tables to a dSYM beside every binary (rustc runs
# dsymutil before it strips). For each binary given, this finds the dSYM under
# target/ whose UUID matches and copies it to OUT_DIR as <name>.dSYM. Matching by
# UUID rather than by path keeps a stale dSYM from an older cached build out.
#
# Usage: scripts/collect-dsyms.sh OUT_DIR BINARY...
# Exits non-zero, naming the binary, when one has no matching dSYM.

if [ "$#" -lt 2 ]; then
  echo "usage: $0 OUT_DIR BINARY..." >&2
  exit 2
fi
OUT_DIR="$1"
shift
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

dsyms=()
while IFS= read -r -d '' dsym; do
  dsyms+=("$dsym")
done < <(find "${REPO_DIR}/target" -type d -name '*.dSYM' -prune -print0 2>/dev/null)
if [ "${#dsyms[@]}" -eq 0 ]; then
  echo "error: no dSYM under target/; is the dist profile still writing them?" >&2
  exit 1
fi

rm -rf -- "$OUT_DIR"
mkdir -p -- "$OUT_DIR"
for binary in "$@"; do
  name="$(basename "$binary")"
  uuid="$(dwarfdump --uuid "$binary" | awk 'NR == 1 {print $2}')"
  if [ -z "$uuid" ]; then
    echo "error: ${binary} has no UUID" >&2
    exit 1
  fi
  match=""
  for dsym in "${dsyms[@]}"; do
    if dwarfdump --uuid "$dsym" 2>/dev/null | grep -q "UUID: ${uuid} "; then
      match="$dsym"
      break
    fi
  done
  if [ -z "$match" ]; then
    echo "error: no dSYM under target/ matches ${name} (${uuid})" >&2
    exit 1
  fi
  ditto "$match" "${OUT_DIR}/${name}.dSYM"
  echo "${name} ${uuid} <- ${match#"${REPO_DIR}"/}"
done
