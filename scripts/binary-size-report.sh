#!/bin/bash
set -euo pipefail

# ── Report what the shipped Mach-O binaries are made of ───────────────
#
# Builds the dist profile with symbols kept, attributes each binary's code to
# crates with cargo-bloat, then strips a copy with `strip -x` (what rustc runs
# for strip = "symbols" on Apple targets) to report the size that ships, plus
# its LZMA size as a stand-in for its share of the ULMO DMG.
#
# CI only: this is a full dist build. Needs cargo-bloat on PATH.
#
# Usage: scripts/binary-size-report.sh [crates-per-binary]

TOP="${1:-25}"
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="${REPO_DIR}/target/binary-size"
SUMMARY="${GITHUB_STEP_SUMMARY:-/dev/null}"

# package:binary for every Mach-O that ships in Fastab.app.
BINARIES=(
  "fastab_desktop:fastab"
  "fastab_cli:ftab"
  "fastab_term:fterm"
  "fastab_input_method:fastab_input_method"
)

# Symbols are what cargo-bloat attributes; the shipped binary drops them below.
export CARGO_PROFILE_DIST_STRIP=none

mkdir -p "$OUT_DIR"
cd "$REPO_DIR"

mb() { python3 -c "print(f'{$1 / 1e6:.2f} MB')"; }

{
  echo "## Shipped binary sizes"
  echo
  echo "| Binary | Stripped | LZMA |"
  echo "| --- | ---: | ---: |"
} >>"$SUMMARY"

for entry in "${BINARIES[@]}"; do
  package="${entry%%:*}"
  binary="${entry##*:}"

  cargo bloat --locked --profile dist -p "$package" --bin "$binary" --crates -n "$TOP" \
    | tee "$OUT_DIR/$binary.crates.txt"

  stripped="$OUT_DIR/$binary"
  cp "target/dist/$binary" "$stripped"
  strip -x "$stripped"
  size="$(stat -f%z "$stripped")"
  lzma_size="$(python3 -c 'import lzma, sys; print(len(lzma.compress(open(sys.argv[1], "rb").read(), preset=6)))' "$stripped")"

  echo "| \`$binary\` | $(mb "$size") | $(mb "$lzma_size") |" >>"$SUMMARY"

  # Annotations are readable from the public API without downloading logs.
  # Workflow commands escape % and newlines in the message; the title is a
  # property, so it must also stay free of commas and colons.
  table="$(sed -n '1,'"$((TOP + 3))"'p' "$OUT_DIR/$binary.crates.txt" \
    | awk '{ gsub(/%/, "%25"); printf "%s%%0A", $0 }')"
  echo "::notice title=$binary $(mb "$size") stripped / $(mb "$lzma_size") LZMA::${table}"
done

{
  echo
  for entry in "${BINARIES[@]}"; do
    binary="${entry##*:}"
    echo "<details><summary><code>$binary</code> by crate</summary>"
    echo
    echo '```'
    cat "$OUT_DIR/$binary.crates.txt"
    echo '```'
    echo
    echo "</details>"
  done
} >>"$SUMMARY"
