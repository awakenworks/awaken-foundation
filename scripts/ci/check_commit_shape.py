#!/usr/bin/env python3
"""Validate a commit message against the project convention.

Usage:
  check_commit_shape.py COMMIT_MSG_FILE   validate one message file (commit-msg)
  check_commit_shape.py --rewrite-stdin   validate rebased commits (post-rewrite)

Format: ``<emoji> <type>(<scope>): <subject>``
- at most 3 lines total — a subject, a blank line, and one body line. Keep the
  log terse; rationale belongs in the code and docs, not the commit message.
- subject <= 100 chars, with a blank line before any body
- no AI-generation markers, ``Co-Authored-By:`` trailers, or external-tool
  provenance markers (e.g. ``via [Tool](https://…)``, ``Tool:``, ``Platform:``)
- no project-management vocabulary

Mirrors the family standard — ../awaken-next (lefthook ``commit-msg``) and
../oversight-next (``scripts/ci/check-commit-message.py``) — and keeps the
foundation tier's line budget tighter (3 vs 4).
"""

from __future__ import annotations

import re
import subprocess
import sys

TYPES = (
    "feat",
    "fix",
    "refactor",
    "docs",
    "test",
    "chore",
    "perf",
    "style",
    "build",
    "ci",
    "revert",
)

MAX_LINES = 3

HEADER = re.compile(
    r"^\S+ (?:" + "|".join(TYPES) + r")(?:\([a-z0-9.-]+\))?: .+"
)

# AI-generation and provenance markers, banned family-wide.
FORBIDDEN = re.compile(
    r"co-authored-by:|generated (with|by)|🤖",
    re.IGNORECASE,
)
# External-tool provenance (e.g. `via [HAPI](https://hapi.run)`), per line.
EXTERNAL_TOOL = re.compile(
    r"(^via \[[^]]+\]\(https?://[^)]+\)$)"
    r"|(^via https?://)"
    r"|(^Tool: [A-Za-z0-9._-]+$)"
    r"|(^Platform: [A-Za-z0-9._-]+$)",
    re.IGNORECASE | re.MULTILINE,
)
# Project-management vocabulary the log must stay free of.
PM_TERMS = re.compile(
    r"(Phases? [0-9])|(Stages? [0-9])|(Steps? [0-9])|(Week [0-9])|(Day [0-9])"
    r"|([Ii]n [Pp]rogress)|([0-9]+% done)|(Sprint [0-9])|(Milestone)"
    r"|(est\.)|(estimated)|(预计)|(计划)|(负责人)|(工作量)|(Owner)|(Assignee)"
)


def validate_text(text: str, label: str = "commit message") -> list[str]:
    """Return a list of violation strings for one commit message (empty = ok)."""
    lines = text.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    # Ignore comment lines git includes in the template, then trailing blanks.
    content = [ln for ln in lines if not ln.startswith("#")]
    while content and content[-1].strip() == "":
        content.pop()
    if not content:
        return [f"{label}: empty commit message"]

    errors: list[str] = []
    header = content[0]
    body = "\n".join(content)

    if not HEADER.match(header):
        errors.append(
            f"{label}: header must be `<emoji> <type>(<scope>): <subject>` "
            f"with type in {{{', '.join(TYPES)}}}"
        )
    if len(header) > 100:
        errors.append(f"{label}: header is {len(header)} chars (max 100)")
    if len(content) > MAX_LINES:
        errors.append(
            f"{label}: message is {len(content)} lines (max {MAX_LINES}): subject, "
            "a blank line, then one body line — keep it terse"
        )
    if len(content) > 1 and content[1].strip():
        errors.append(f"{label}: leave a blank line between subject and body")
    if FORBIDDEN.search(body):
        errors.append(f"{label}: AI-generation or Co-Authored-By marker not allowed")
    if EXTERNAL_TOOL.search(body):
        errors.append(f"{label}: external-tool provenance marker not allowed (e.g. `via [Tool](url)`)")
    if PM_TERMS.search(body):
        errors.append(f"{label}: project-management term not allowed")
    return errors


def validate_file(path: str) -> list[str]:
    try:
        with open(path, encoding="utf-8") as handle:
            return validate_text(handle.read(), path)
    except OSError as exc:
        return [f"{path}: cannot read commit message file: {exc}"]


def validate_rewrite_stdin() -> list[str]:
    """Validate the new commits of a rebase, read as old->new SHA pairs on stdin."""
    # A TTY stdin would block forever (hook runner did not forward git's pairs);
    # there is nothing to validate, so return cleanly instead of hanging.
    if sys.stdin.isatty():
        return []
    errors: list[str] = []
    for raw in sys.stdin:
        parts = raw.split()
        if len(parts) < 2:
            continue
        new_sha = parts[1]
        result = subprocess.run(
            ["git", "log", "-1", "--format=%B", new_sha],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if result.returncode == 0 and result.stdout:
            errors.extend(validate_text(result.stdout, new_sha))
    return errors


def main(argv: list[str]) -> int:
    if not argv:
        print("check-commit-shape: usage: COMMIT_MSG_FILE | --rewrite-stdin", file=sys.stderr)
        return 1

    if argv[0] == "--rewrite-stdin":
        errors = validate_rewrite_stdin()
    else:
        errors = validate_file(argv[0])

    if errors:
        print("check-commit-shape:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
