#!/usr/bin/env python3
"""Tests for check_changesets.py.

Run with: python3 scripts/test_check_changesets.py
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import check_changesets as cc

PACKAGES = {name: cc.BUMP_TYPES for name in ("coding", "harnx", "pantheon")}


def problems(text: str, packages=PACKAGES) -> list[cc.Problem]:
    return cc.check_changeset(".changeset/x.md", text, packages)


def messages(text: str, packages=PACKAGES) -> list[str]:
    return [problem.message for problem in problems(text, packages)]


class TestCheckChangeset(unittest.TestCase):
    def test_valid_changeset_passes(self) -> None:
        self.assertEqual(messages("---\nharnx: patch\n---\n\nFix a thing.\n"), [])

    def test_several_packages_and_crlf_pass(self) -> None:
        text = "---\r\nharnx: minor\r\npantheon: patch\r\n---\r\nAdd a thing.\r\n"
        self.assertEqual(messages(text), [])

    def test_quoted_key_is_rejected_with_its_line(self) -> None:
        found = problems('---\nharnx: patch\n"pantheon": minor\n---\nText.\n')
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0].line, 3)
        self.assertIn("quoted key", found[0].message)
        self.assertIn("pantheon: minor", found[0].message)

    def test_single_quoted_key_is_rejected(self) -> None:
        self.assertIn("quoted key", messages("---\n'harnx': patch\n---\nText.\n")[0])

    def test_crate_name_is_rejected_quoted_or_not(self) -> None:
        for key in ("harnx-runtime", '"harnx-runtime"'):
            with self.subTest(key=key):
                found = messages(f"---\n{key}: patch\n---\nText.\n")
                self.assertEqual(len(found), 1)
                self.assertIn("released as harnx", found[0])

    def test_unknown_package_is_rejected(self) -> None:
        found = messages("---\nweb: patch\n---\nText.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("coding, harnx, pantheon", found[0])

    def test_change_types_knope_would_drop_are_rejected(self) -> None:
        for value in ("Patch", '"patch"', "patch # a comment", "fix", ""):
            with self.subTest(value=value):
                found = messages(f"---\nharnx: {value}\n---\nText.\n")
                self.assertEqual(len(found), 1)
                self.assertIn("not one of major, minor, patch", found[0])

    def test_extra_changelog_section_type_is_accepted(self) -> None:
        packages = {"harnx": cc.BUMP_TYPES | {"note"}}
        self.assertEqual(messages("---\nharnx: note\n---\nText.\n", packages), [])

    def test_duplicate_key_is_rejected(self) -> None:
        found = messages("---\nharnx: patch\nharnx: minor\n---\nText.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("listed twice", found[0])

    def test_line_without_a_colon_is_rejected(self) -> None:
        self.assertIn("expected", messages("---\nharnx patch\n---\nText.\n")[0])

    def test_missing_front_matter_is_rejected(self) -> None:
        found = problems("harnx: patch\n\nText.\n")
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0].line, 1)
        self.assertIn("no front matter", found[0].message)

    def test_unclosed_front_matter_is_rejected(self) -> None:
        self.assertIn("no closing", messages("---\nharnx: patch\n\nText.\n")[0])

    def test_empty_front_matter_is_rejected(self) -> None:
        found = messages("---\n---\nText.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("names no package", found[0])
        self.assertIn("fails the release", found[0])

    def test_blank_lines_in_front_matter_are_rejected_with_their_lines(self) -> None:
        cases = {
            "---\n\nharnx: patch\n---\nText.\n": 2,
            "---\nharnx: patch\n\npantheon: patch\n---\nText.\n": 3,
            "---\nharnx: patch\n  \n---\nText.\n": 3,
            "---\r\nharnx: patch\r\n\r\n---\r\nText.\r\n": 3,
        }
        for text, line in cases.items():
            with self.subTest(text=text):
                found = problems(text)
                self.assertEqual([problem.line for problem in found], [line])
                self.assertIn("blank line", found[0].message)

    def test_byte_order_mark_is_rejected(self) -> None:
        found = messages("﻿---\nharnx: patch\n---\nText.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("byte-order mark", found[0])

    def test_empty_body_is_rejected(self) -> None:
        for text in ("---\nharnx: patch\n---\n", "---\nharnx: patch\n---\n\n  \n"):
            with self.subTest(text=text):
                found = messages(text)
                self.assertEqual(len(found), 1)
                self.assertIn("no description", found[0])


class TestLayout(unittest.TestCase):
    def test_misnamed_directories_are_reported_once(self) -> None:
        found = cc.misplaced_directories(
            [
                ".changeset/ok.md",
                ".changesets/a.md",
                ".changesets/b.md",
                "changeset/c.md",
                "packages/pantheon/.changeset/d.md",
                "crates/harnx/src/changeset.rs",
            ]
        )
        self.assertEqual(
            [problem.path for problem in found],
            [".changesets/", "changeset/", "packages/pantheon/.changeset/"],
        )

    def test_code_directories_named_changeset_below_the_root_are_allowed(self) -> None:
        paths = [
            "crates/harnx-core/src/changeset/mod.rs",
            "scripts/fixtures/changesets/example.md",
            "web/src/components/ChangeSet/index.tsx",
        ]
        self.assertEqual(cc.misplaced_directories(paths), [])

    def test_files_knope_skips_inside_changeset_dir_are_reported(self) -> None:
        found = cc.stray_files(
            [
                ".changeset/.gitkeep",
                ".changeset/ok.md",
                ".changeset/notes.txt",
                ".changeset/sub/nested.md",
                "docs/notes.txt",
            ]
        )
        self.assertEqual(
            [problem.path for problem in found],
            [".changeset/notes.txt", ".changeset/sub/nested.md"],
        )


class TestRepository(unittest.TestCase):
    def test_load_packages_reads_knope_toml(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            knope_toml = Path(tmp) / "knope.toml"
            knope_toml.write_text(
                "[packages.harnx]\n"
                'changelog = "CHANGELOG.md"\n'
                'extra_changelog_sections = [{ name = "Notes", types = ["note"] }]\n'
                "[packages.coding]\n"
                'changelog = "packages/coding/CHANGELOG.md"\n',
                encoding="utf-8",
            )
            self.assertEqual(
                cc.load_packages(knope_toml),
                {"harnx": cc.BUMP_TYPES | {"note"}, "coding": cc.BUMP_TYPES},
            )

    def test_check_repo_covers_untracked_files_and_skips_ignored_ones(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / "knope.toml").write_text("[packages.harnx]\n", encoding="utf-8")
            (root / ".gitignore").write_text("target/\n", encoding="utf-8")
            for path, text in {
                ".changeset/good.md": "---\nharnx: patch\n---\nText.\n",
                ".changeset/bad.md": '---\n"harnx": patch\n---\nText.\n',
                ".changesets/lost.md": "---\nharnx: patch\n---\nText.\n",
                "target/.changesets/ignored.md": "---\nharnx: patch\n---\nText.\n",
            }.items():
                (root / path).parent.mkdir(parents=True, exist_ok=True)
                (root / path).write_text(text, encoding="utf-8")

            found = cc.check_repo(root)

            self.assertEqual(
                [(problem.path, problem.line) for problem in found],
                [(".changesets/", None), (".changeset/bad.md", 2)],
            )


class TestOutput(unittest.TestCase):
    def test_problem_formats_with_and_without_a_line(self) -> None:
        self.assertEqual(str(cc.Problem("a.md", 2, "bad")), "a.md:2: bad")
        self.assertEqual(str(cc.Problem("dir/", None, "bad")), "dir/: bad")

    def test_annotation_escapes_workflow_command_characters(self) -> None:
        problem = cc.Problem(".changeset/a,b.md", 2, "100% wrong\nreally")
        self.assertEqual(
            problem.annotation(),
            "::error file=.changeset/a%2Cb.md,line=2::100%25 wrong%0Areally",
        )


if __name__ == "__main__":
    unittest.main()
