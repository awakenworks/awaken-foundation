#!/usr/bin/env python3
"""Validate a commit message against the project convention.

Usage: check_commit_shape.py COMMIT_MSG_FILE

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


def main(argv: list[str]) -> int:
    if not argv:
        print("check-commit-shape: missing commit message file", file=sys.stderr)
        return 1

    with open(argv[0], encoding="utf-8") as handle:
        raw = handle.read()

    lines = raw.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    # Ignore comment lines git includes in the template, then trailing blanks.
    content = [ln for ln in lines if not ln.startswith("#")]
    while content and content[-1].strip() == "":
        content.pop()
    if not content:
        print("check-commit-shape: empty commit message", file=sys.stderr)
        return 1

    errors: list[str] = []
    header = content[0]
    text = "\n".join(content)

    if not HEADER.match(header):
        errors.append(
            "header must be `<emoji> <type>(<scope>): <subject>` "
            f"with type in {{{', '.join(TYPES)}}}"
        )
    if len(header) > 100:
        errors.append(f"header is {len(header)} chars (max 100)")
    if len(content) > MAX_LINES:
        errors.append(
            f"message is {len(content)} lines (max {MAX_LINES}): subject, a blank "
            "line, then one body line — keep it terse"
        )
    if len(content) > 1 and content[1].strip():
        errors.append("leave a blank line between subject and body")

    if FORBIDDEN.search(text):
        errors.append("AI-generation or Co-Authored-By marker not allowed")
    if EXTERNAL_TOOL.search(text):
        errors.append("external-tool provenance marker not allowed (e.g. `via [Tool](url)`)")
    if PM_TERMS.search(text):
        errors.append("project-management term not allowed")

    if errors:
        print("check-commit-shape:", file=sys.stderr)
        for e in errors:
            print(f"  {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
