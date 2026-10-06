#!/usr/bin/env python3
"""Fail when a changeset would not reach a changelog.

knope (see knope.toml) builds each release's changelogs from `.changeset/*.md`,
and it drops several kinds of mistake without a warning:

- It reads only `.md` files directly inside `.changeset/` at the repository
  root. A file in `.changesets/`, in a subdirectory or with another extension
  is never read, so it stays behind forever.
- It skips a quoted key (`"harnx": patch`) and a key that is not a knope
  package, such as a crate name. A file with no usable key is never consumed;
  one that also has a usable key is consumed and deleted, and the skipped
  key's entry is lost.
- It deletes a file whose change type is not exactly one it knows (`Patch`,
  `"patch"`, `patch # comment`) without adding that key's entry.
- It turns an empty description into an empty bullet.

It also rejects what stops the whole release with an error that doesn't name
the file: a blank line or comment in the front matter, a duplicate key, empty
front matter, a byte-order mark, or no front matter at all.

Checked against knope 0.23.0. Run with: python3 scripts/check_changesets.py
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

try:
    import tomllib
except ImportError:
    sys.exit("check_changesets.py needs Python 3.11 or newer, which has tomllib")

ROOT = Path(__file__).resolve().parent.parent
CHANGESET_DIR = ".changeset"
BUMP_TYPES = frozenset({"major", "minor", "patch"})
# Names someone might give the changeset directory, which knope reads only at
# the repository root. Below the root only a dot-prefixed name counts, because
# a plain `changeset/` there is more likely code, such as a module.
ROOT_DIR_NAME = re.compile(r"\.?change[-_]?sets?", re.IGNORECASE)
NESTED_DIR_NAME = re.compile(r"\.change[-_]?sets?", re.IGNORECASE)
FRONT_MATTER_FENCE = "---"
# knope stops the whole release on these, and its error doesn't say which file.
FAILS_RELEASE = "knope fails the release on it without naming the file"


@dataclass(frozen=True)
class Problem:
    path: str
    line: int | None
    message: str

    def __str__(self) -> str:
        where = f"{self.path}:{self.line}" if self.line else self.path
        return f"{where}: {self.message}"

    def annotation(self) -> str:
        """Render as a GitHub Actions error, which shows on the pull request diff."""
        properties = f"file={_escape(self.path, property_value=True)}"
        if self.line:
            properties += f",line={self.line}"
        return f"::error {properties}::{_escape(self.message)}"


def _escape(text: str, property_value: bool = False) -> str:
    text = text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    if property_value:
        text = text.replace(":", "%3A").replace(",", "%2C")
    return text


def load_packages(knope_toml: Path) -> dict[str, frozenset[str]]:
    """Map each knope package to the change types its changesets may use."""
    config = tomllib.loads(knope_toml.read_text(encoding="utf-8"))
    packages = config.get("packages")
    if not packages:
        raise SystemExit(f"{knope_toml}: no [packages.<name>] tables")
    return {
        name: BUMP_TYPES | _extra_change_types(package)
        for name, package in packages.items()
    }


def _extra_change_types(package: dict) -> frozenset[str]:
    sections = package.get("extra_changelog_sections", [])
    return frozenset(kind for section in sections for kind in section.get("types", []))


def misplaced_directories(paths: list[str]) -> list[Problem]:
    """Report directories named like `.changeset` that knope never reads."""
    found = set()
    for path in paths:
        parts = path.split("/")
        for depth, name in enumerate(parts[:-1]):
            if _is_misplaced_changeset_dir(depth, name):
                found.add("/".join(parts[: depth + 1]) + "/")
    return [
        Problem(
            directory,
            None,
            f"knope reads changesets only from {CHANGESET_DIR}/ at the repository "
            "root, so nothing here reaches a changelog; move the files there",
        )
        for directory in sorted(found)
    ]


def _is_misplaced_changeset_dir(depth: int, name: str) -> bool:
    if depth == 0:
        return name != CHANGESET_DIR and ROOT_DIR_NAME.fullmatch(name) is not None
    return NESTED_DIR_NAME.fullmatch(name) is not None


def stray_files(paths: list[str]) -> list[Problem]:
    """Report files inside `.changeset/` that knope skips."""
    problems = []
    for path in paths:
        parts = path.split("/")
        if parts[0] != CHANGESET_DIR or path == f"{CHANGESET_DIR}/.gitkeep":
            continue
        if len(parts) > 2:
            message = f"knope does not read subdirectories of {CHANGESET_DIR}/; move this file up into it"
        elif not path.endswith(".md"):
            message = "knope reads only .md files; rename it to end in .md"
        else:
            continue
        problems.append(Problem(path, None, message))
    return problems


def check_changeset(path: str, text: str, packages: dict[str, frozenset[str]]) -> list[Problem]:
    """Report everything in one changeset that knope would skip, lose or fail on."""
    try:
        front_matter, fence_line, body = _split_front_matter(text)
    except ValueError as error:
        return [Problem(path, 1, str(error))]
    problems = _check_front_matter(path, front_matter, packages)
    if not body.strip():
        problems.append(
            Problem(path, fence_line, "no description after the front matter; knope would add an empty bullet")
        )
    return problems


def _split_front_matter(text: str) -> tuple[list[str], int, str]:
    """Return the front matter's lines, the closing fence's line number and the body."""
    if text.startswith("﻿"):
        raise ValueError(f"the file starts with a byte-order mark; {FAILS_RELEASE}, so save it without one")
    lines = text.replace("\r\n", "\n").split("\n")
    if lines[0].strip() != FRONT_MATTER_FENCE:
        raise ValueError(
            'no front matter; start the file with a "---" line, a line such as '
            '"harnx: patch", and another "---" line'
        )
    for end in range(1, len(lines)):
        if lines[end].strip() == FRONT_MATTER_FENCE:
            return lines[1:end], end + 1, "\n".join(lines[end + 1 :])
    raise ValueError('front matter has no closing "---" line')


def _check_front_matter(path: str, lines: list[str], packages: dict[str, frozenset[str]]) -> list[Problem]:
    if not any(line.strip() for line in lines):
        return [Problem(path, 1, f"front matter names no package; {FAILS_RELEASE}")]
    problems = []
    seen: set[str] = set()
    for number, line in enumerate(lines, start=2):
        message = _check_front_matter_line(line, packages, seen)
        if message:
            problems.append(Problem(path, number, message))
    return problems


def _check_front_matter_line(line: str, packages: dict[str, frozenset[str]], seen: set[str]) -> str | None:
    if not line.strip():
        return f"blank line in the front matter; {FAILS_RELEASE}"
    key, colon, value = line.partition(":")
    key, value = key.strip(), value.strip()
    if not colon or not key.strip("\"'"):
        return f'expected "<package>: <change type>", found {line.strip()!r}'
    return _check_key(key, value, packages) or _check_value(key, value, packages, seen)


def _check_key(key: str, value: str, packages: dict[str, frozenset[str]]) -> str | None:
    bare = key.strip("\"'")
    if bare not in packages:
        if bare.startswith("harnx-"):
            return f"{key} is a crate, not a knope package; every workspace crate is released as harnx"
        return f"{key} is not a knope package; use one of {', '.join(sorted(packages))}"
    if bare != key:
        return f"quoted key {key}; knope skips quoted keys without a warning, so write {bare}: {value}"
    return None


def _check_value(key: str, value: str, packages: dict[str, frozenset[str]], seen: set[str]) -> str | None:
    if key in seen:
        return f"{key} is listed twice; {FAILS_RELEASE}"
    seen.add(key)
    if value not in packages[key]:
        return (
            f"change type {value!r} is not one of {', '.join(sorted(packages[key]))}; "
            "knope would delete this file without adding this entry to a changelog"
        )
    return None


def candidate_paths(root: Path) -> list[str]:
    """List the files git tracks or would track, which leaves out build output."""
    listing = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    # A file deleted from the working tree but not yet from the index is gone.
    return sorted({path for path in listing.split("\0") if path and (root / path).is_file()})


def is_changeset(path: str) -> bool:
    """Whether knope reads this file as a changeset."""
    return PurePosixPath(path).parent == PurePosixPath(CHANGESET_DIR) and path.endswith(".md")


def check_repo(root: Path) -> list[Problem]:
    packages = load_packages(root / "knope.toml")
    paths = candidate_paths(root)
    problems = misplaced_directories(paths) + stray_files(paths)
    for path in filter(is_changeset, paths):
        text = (root / path).read_text(encoding="utf-8")
        problems += check_changeset(path, text, packages)
    return problems


def main() -> int:
    problems = check_repo(ROOT)
    in_github_actions = os.environ.get("GITHUB_ACTIONS") == "true"
    for problem in problems:
        print(problem.annotation() if in_github_actions else problem)
    sys.stdout.flush()
    if problems:
        print(
            f"\n{len(problems)} changeset problem(s). The format is described under "
            '"Changeset Files" in README.md.',
            file=sys.stderr,
        )
        return 1
    print("Changesets OK.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
