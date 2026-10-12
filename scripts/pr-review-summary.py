#!/usr/bin/env python3
"""List what a reviewer should read closely in a pull request.

Usage: scripts/pr-review-summary.py BASE HEAD

Compares two commits and writes Markdown to $GITHUB_STEP_SUMMARY, or to
standard output when that is unset: build scripts added or changed, crates
new to Cargo.lock and which of them are proc macros, added `unsafe`, added
binary files, and added lines that are very long or look like base64.

It exits 1 on a hard violation: a binary file added outside docs/,
app/data/ and testmail/, or a crate in Cargo.lock from anywhere but
crates.io. Everything else is listed for a person to judge.
"""

import json
import os
import re
import subprocess
import sys
import tomllib

CRATES_IO = {
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
}
BINARY_ALLOWED = ("docs/", "app/data/", "testmail/")
LONG_LINE = 500
PROSE = (".md", ".po", ".pot", ".txt")
BASE64_RUN = re.compile(r"[A-Za-z0-9+/]{200,}={0,2}")
UNSAFE = re.compile(r"\bunsafe\s*(\{|\(|fn\b|impl\b|extern\b|trait\b)")
LIMIT = 50


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True, errors="replace"
    ).stdout


def git_or_empty(*args: str) -> str:
    result = subprocess.run(["git", *args], capture_output=True, text=True, errors="replace")
    return result.stdout if result.returncode == 0 else ""


def lock_packages(text: str) -> set[tuple[str, str, str]]:
    if not text:
        return set()
    lock = tomllib.loads(text)
    return {
        (p["name"], p["version"], p.get("source", "(path)"))
        for p in lock.get("package", [])
    }


def proc_macro_names() -> set[str] | None:
    """Names of proc-macro crates in the checked-out workspace, or None."""
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    meta = json.loads(result.stdout)
    return {
        f"{p['name']} {p['version']}"
        for p in meta["packages"]
        if any("proc-macro" in t["kind"] for t in p["targets"])
    }


def added_lines(base: str, head: str) -> list[tuple[str, int, str]]:
    """(path, line number, text) for every line the diff adds to a text file."""
    diff = git("diff", "--no-color", "--no-ext-diff", "-U0", base, head)
    out = []
    path = None
    line_no = 0
    for raw in diff.splitlines():
        if raw.startswith("+++ "):
            path = raw[6:] if raw.startswith("+++ b/") else None
        elif raw.startswith("@@"):
            m = re.search(r"\+(\d+)", raw)
            line_no = int(m.group(1)) if m else 0
        elif raw.startswith("+") and path:
            out.append((path, line_no, raw[1:]))
            line_no += 1
    return out


def section(title: str, rows: list[str], empty: str = "None.") -> str:
    body = "\n".join(f"- {r}" for r in rows[:LIMIT]) if rows else empty
    if len(rows) > LIMIT:
        body += f"\n- ...and {len(rows) - LIMIT} more"
    return f"### {title}\n\n{body}\n"


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    base, head = sys.argv[1], sys.argv[2]
    errors = []

    status = git("diff", "--name-status", "--no-renames", base, head)
    changed = [line.split("\t", 1) for line in status.splitlines() if "\t" in line]
    build_scripts = [
        f"`{p}` ({'added' if s == 'A' else 'deleted' if s == 'D' else 'changed'})"
        for s, p in changed
        if p == "build.rs" or p.endswith("/build.rs")
    ]

    old = lock_packages(git_or_empty("show", f"{base}:Cargo.lock"))
    new = lock_packages(git_or_empty("show", f"{head}:Cargo.lock"))
    old_names = {(n, s) for n, _, s in old}
    added = sorted(new - old)
    crates = [
        f"`{n}` {v} from {s}" + ("" if (n, s) in old_names else ", new name")
        for n, v, s in added
    ]
    for n, v, s in sorted(new):
        if s != "(path)" and s not in CRATES_IO:
            errors.append(f"`{n}` {v} comes from {s}, not crates.io")

    macros: list[str] = []
    macro_note = ""
    if added:
        names = proc_macro_names()
        if names is None:
            macro_note = "cargo metadata failed, so proc macros were not checked."
        else:
            macros = [f"`{n}` {v}" for n, v, _ in added if f"{n} {v}" in names]

    numstat = git("diff", "--numstat", "--no-renames", base, head)
    added_paths = {p for s, p in changed if s == "A"}
    binaries = []
    for line in numstat.splitlines():
        a, d, path = line.split("\t", 2)
        if a == "-" and d == "-" and path in added_paths:
            binaries.append(f"`{path}`")
            if not path.startswith(BINARY_ALLOWED):
                errors.append(f"binary file `{path}` added outside docs/, app/data/ and testmail/")

    unsafe_rows = []
    long_rows = []
    for path, no, text in added_lines(base, head):
        if path.endswith(".rs") and UNSAFE.search(text):
            unsafe_rows.append(f"`{path}:{no}`: `{text.strip()[:120]}`")
        if path == "Cargo.lock":
            continue
        # Prose files hold whole paragraphs on one line.
        if len(text) > LONG_LINE and not path.endswith(PROSE):
            long_rows.append(f"`{path}:{no}`: {len(text)} characters")
        elif BASE64_RUN.search(text):
            long_rows.append(f"`{path}:{no}`: a run that looks like base64")

    out = [f"## Pull request review notes\n\nCompared `{base[:12]}` with `{head[:12]}`.\n"]
    if errors:
        out.append(section("Hard failures", errors))
    out.append(section("Build scripts added or changed", build_scripts))
    out.append(section("Crates new to Cargo.lock", crates))
    out.append(section("New proc-macro crates", macros, macro_note or "None."))
    out.append(section("Added `unsafe`", unsafe_rows))
    out.append(section("Binary files added", binaries))
    out.append(section("Very long or base64-looking added lines", long_rows))
    report = "\n".join(out)

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(report)
    else:
        print(report)
    for e in errors:
        # Paths come from the pull request; a newline in one must not start
        # a second workflow command.
        clean = e.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"::error::{clean}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
