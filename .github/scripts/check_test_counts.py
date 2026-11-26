#!/usr/bin/env python3
"""Fail if README.md's per-crate test table disagrees with the tree.

Counts anchored test declarations and compares them with the table under
"Crates". Exits 1 on any mismatch, a missing crate, or a crate in the table
that has no tests, so the table cannot drift silently again: it read 345
while the tree held 432, and no test failed because of it.

Run from the repo root:

    python3 .github/scripts/check_test_counts.py

Needs only git and the standard library, so it can also be wired in as a CI
step later without adding a dependency.
"""
import re
import subprocess
import sys
from collections import Counter

ATTR = re.compile(r"^\s*#\[(test|tokio::test|sqlx::test)\b")
ROW = re.compile(r"^\| `(escrow[a-z-]*)` \|")


def counted(root: str = ".") -> Counter:
    files = subprocess.run(
        ["git", "ls-files", "*.rs"],
        cwd=root,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    actual: Counter = Counter()
    for path in files:
        crate = path.split("/")[1] if path.startswith("crates/") else "other"
        with open(f"{root}/{path}", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if ATTR.match(line):
                    actual[crate] += 1
    return actual


def claimed(root: str = ".") -> dict:
    table: dict = {}
    with open(f"{root}/README.md", encoding="utf-8") as fh:
        for line in fh:
            if not ROW.match(line):
                continue
            parts = [p.strip() for p in line.strip().strip("|").split("|")]
            if len(parts) != 3:
                continue
            m = re.match(r"(\d+)", parts[2])
            if m:
                table[parts[0].strip("`")] = int(m.group(1))
    return table


def main() -> int:
    actual, table = counted(), claimed()
    bad = False
    for crate in sorted(set(actual) | set(table)):
        if actual[crate] == 0 and crate not in table:
            continue
        if crate not in table:
            print(f"MISSING from README: {crate} (tree has {actual[crate]})")
            bad = True
        elif table[crate] != actual[crate]:
            print(f"MISMATCH {crate}: README {table[crate]}, tree {actual[crate]}")
            bad = True
    if bad:
        return 1
    print(f"README test table matches the tree ({sum(actual.values())} declarations)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
