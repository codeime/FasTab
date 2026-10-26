#!/usr/bin/env python3
"""Turn a failed CI step's output into GitHub error annotations.

Job logs need a GitHub login to download; check-run annotations are readable
from the public API. A failing step that only says "exit code 101" cannot be
diagnosed without the log, so the steps that tee their output call this on
failure.

  annotate-ci-failure.py cargo-test LOG   one annotation per failing test (panic
                                          text) or compile error
  annotate-ci-failure.py tail LOG [N]     the last N lines (default 60)
"""

import re
import sys

# GitHub keeps at most ten error annotations per step.
MAX_ANNOTATIONS = 10
MAX_LINES = 40


def escape_data(text: str) -> str:
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_property(text: str) -> str:
    return escape_data(text).replace(":", "%3A").replace(",", "%2C")


def annotate(title: str, lines: list[str]) -> None:
    body = "\n".join(lines[:MAX_LINES]) or "(no output)"
    print(f"::error title={escape_property(title)}::{escape_data(body)}")


def cargo_test(lines: list[str]) -> int:
    found = []
    # `---- module::test stdout ----` opens each failing test's captured output.
    current = None
    for line in lines:
        header = re.match(r"^---- (.+?) stdout ----$", line)
        if header:
            current = (header.group(1), [])
            found.append(current)
            continue
        if current is None:
            continue
        if line.startswith("---- ") or line == "failures:" or line.startswith("test result:"):
            current = None
            continue
        current[1].append(line)

    # Compile failures never reach a test harness.
    for index, line in enumerate(lines):
        if re.match(r"^error(\[E\d+\])?: ", line):
            found.append((line[:120], lines[index : index + 15]))

    # A test binary that crashed (abort, SIGSEGV) prints no `----` section.
    for line in lines:
        if "process didn't exit successfully" in line:
            found.append(("test binary did not exit successfully", [line]))

    for title, body in found[:MAX_ANNOTATIONS]:
        annotate(title, body)
    if not found:
        return tail(lines, 60)
    return 0


def tail(lines: list[str], count: int) -> int:
    annotate("last output lines", lines[-count:])
    return 0


def main(argv: list[str]) -> int:
    if len(argv) < 3 or argv[1] not in ("cargo-test", "tail"):
        print(__doc__, file=sys.stderr)
        return 2
    with open(argv[2], encoding="utf-8", errors="replace") as handle:
        lines = handle.read().splitlines()
    if argv[1] == "cargo-test":
        return cargo_test(lines)
    return tail(lines, int(argv[3]) if len(argv) > 3 else 60)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
