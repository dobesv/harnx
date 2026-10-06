"""Strict, per-test waivers for the pinned A2A TCK."""

import tomllib
from pathlib import Path

import pytest

CATEGORIES = {"design-conflict", "tck-defect", "harness-limitation"}


def load_waivers():
    path = Path(__file__).with_name("waivers.toml")
    try:
        with path.open("rb") as source:
            waivers = tomllib.load(source).get("waiver", [])
    except (OSError, ValueError, KeyError) as error:
        raise pytest.UsageError(f"A2A waivers: cannot read {path}: {error}") from error
    if not isinstance(waivers, list):
        raise pytest.UsageError("A2A waivers: expected [[waiver]] entries")
    tests = set()
    for waiver in waivers:
        validate_waiver(waiver)
        test = waiver["test"]
        if test in tests:
            raise pytest.UsageError(f"A2A waivers: duplicate test: {test}")
        tests.add(test)
    return waivers


def validate_waiver(waiver):
    if not isinstance(waiver, dict):
        raise pytest.UsageError("A2A waivers: each entry must be a table")
    for field in ("test", "category", "reason"):
        value = waiver.get(field)
        if not isinstance(value, str) or not value.strip():
            raise pytest.UsageError(f"A2A waivers: missing or empty {field}: {waiver}")
    if waiver["category"] not in CATEGORIES:
        raise pytest.UsageError(f"A2A waivers: invalid category: {waiver['category']}")


@pytest.hookimpl(tryfirst=True)
def pytest_collection_modifyitems(items):
    # Check the complete collection before pytest applies -m/-k deselection.
    by_nodeid = {item.nodeid: item for item in items}
    for waiver in load_waivers():
        test = waiver["test"]
        if test not in by_nodeid:
            raise pytest.UsageError(f"A2A waiver matched no collected test: {test}")
        reason = f"{waiver['category']}: {waiver['reason']}"
        # Prepend so an existing non-strict xfail cannot weaken this waiver.
        by_nodeid[test].add_marker(
            pytest.mark.xfail(strict=True, reason=reason), append=False
        )
