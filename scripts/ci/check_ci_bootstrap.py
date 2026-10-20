#!/usr/bin/env python3
"""Check the Python and checkout prerequisites before native CI starts."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def violations(workflow: str, attributes: str) -> list[str]:
    errors = []
    for name in ("native", "security"):
        match = re.search(r"(?ms)^  " + name + r":\n(.*?)(?=^  \S|\Z)", workflow)
        if not match:
            errors.append(name + ": required CI job is missing")
            continue
        body = match.group(1)
        setup = re.search(r"uses: actions/setup-python@[0-9a-f]{40}(?:[ \t]+#[^\n]*)?\s+with:\s+python-version: [\"']3\.12[\"']", body)
        first_python = body.find("python scripts/")
        if not setup or first_python < 0 or setup.end() > first_python:
            errors.append(name + ": install pinned Python 3.12 before the Python gates")
    if "* text=auto eol=lf" not in attributes.splitlines():
        errors.append("Fresh Windows checkouts must preserve the Unix newline policy")
    return errors


class BootstrapTests(unittest.TestCase):
    def test_real_workflow_has_both_prerequisites(self):
        self.assertEqual(violations((ROOT / ".github/workflows/quality.yml").read_text(encoding="utf-8"),
                                    (ROOT / ".gitattributes").read_text(encoding="utf-8")), [])

    def test_missing_old_or_floating_python_and_wrong_line_endings_fail(self):
        text = (ROOT / ".github/workflows/quality.yml").read_text(encoding="utf-8")
        attributes = (ROOT / ".gitattributes").read_text(encoding="utf-8")
        for changed in (text.replace('python-version: "3.12"', 'python-version: "3.10"'),
                        re.sub(r"actions/setup-python@[0-9a-f]{40}", "actions/setup-python@v6", text),
                        text.replace("  native:", "  absent:", 1),
                        text.replace("  security:", "  absent:", 1),
                        text.replace("    steps:\n", "    steps:\n      - run: python scripts/ci/check_trust_roots.py\n", 1),
                        text.split("  security:", 1)[0] + "  security:" + text.split("  security:", 1)[1].replace(
                            'python-version: "3.12"', 'python-version: "3.10"', 1)):
            self.assertTrue(violations(changed, attributes))
        self.assertTrue(violations(text, "* text=auto\n"))

    def test_real_git_checkout_converts_crlf_only_without_the_policy(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0")
        with tempfile.TemporaryDirectory(prefix="foundation checkout ") as directory:
            root = Path(directory).resolve()
            source = root / "source"
            def git(*args):
                return subprocess.check_output(["git", *args], env=env, stderr=subprocess.STDOUT).decode().strip()
            git("init", "--initial-branch=main", str(source))
            (source / "lib.rs").write_bytes(b"pub const VALUE: u8 = 1;\n")
            git("-C", str(source), "add", "lib.rs")
            def commit():
                git("-C", str(source), "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
                    "-c", "commit.gpgsign=false", "commit", "-m", "fixture")
            commit()
            before = git("-C", str(source), "rev-parse", "HEAD:lib.rs")
            git("-c", "core.autocrlf=true", "clone", source.as_uri(), str(root / "without"))
            self.assertIn(b"\r\n", (root / "without/lib.rs").read_bytes())
            (source / ".gitattributes").write_bytes((ROOT / ".gitattributes").read_bytes())
            git("-C", str(source), "add", ".gitattributes")
            commit()
            self.assertEqual(git("-C", str(source), "rev-parse", "HEAD:lib.rs"), before)
            git("-c", "core.autocrlf=true", "clone", source.as_uri(), str(root / "with"))
            self.assertNotIn(b"\r\n", (root / "with/lib.rs").read_bytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(BootstrapTests)).wasSuccessful() else 1
    errors = violations((ROOT / ".github/workflows/quality.yml").read_text(encoding="utf-8"),
                        (ROOT / ".gitattributes").read_text(encoding="utf-8"))
    if os.environ.get("GITHUB_ACTIONS") == "true" and sys.version_info[:2] != (3, 12):
        errors.append("CI did not select the declared Python 3.12 runtime")
    for error in errors:
        print("ERROR: " + error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
