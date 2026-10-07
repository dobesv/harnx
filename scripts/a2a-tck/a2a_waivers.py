"""Strict, per-test waivers for the pinned A2A TCK."""

import re
import tomllib
from pathlib import Path

import pytest

CATEGORIES = {"design-conflict", "tck-defect", "harness-limitation"}
WAIVER_PATTERN = pytest.StashKey[re.Pattern[str]]()


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
    validated = []
    for waiver in waivers:
        pattern = validate_waiver(waiver)
        test = waiver["test"]
        if test in tests:
            raise pytest.UsageError(f"A2A waivers: duplicate test: {test}")
        tests.add(test)
        validated.append((waiver, pattern))
    return validated


def require_field(waiver, field):
    value = waiver.get(field)
    if not isinstance(value, str) or not value.strip():
        raise pytest.UsageError(f"A2A waivers: missing or empty {field}: {waiver}")


def validate_waiver(waiver):
    if not isinstance(waiver, dict):
        raise pytest.UsageError("A2A waivers: each entry must be a table")
    for field in ("test", "category", "reason", "match"):
        require_field(waiver, field)
    if waiver["category"] not in CATEGORIES:
        raise pytest.UsageError(f"A2A waivers: invalid category: {waiver['category']}")
    try:
        return re.compile(waiver["match"])
    except re.error as error:
        raise pytest.UsageError(
            f"A2A waivers: invalid match regex for {waiver['test']}: {error}"
        ) from error


@pytest.hookimpl(tryfirst=True)
def pytest_collection_modifyitems(items):
    # Check the complete collection before pytest applies -m/-k deselection.
    by_nodeid = {item.nodeid: item for item in items}
    for waiver, pattern in load_waivers():
        test = waiver["test"]
        if test not in by_nodeid:
            raise pytest.UsageError(f"A2A waiver matched no collected test: {test}")
        by_nodeid[test].stash[WAIVER_PATTERN] = pattern
        reason = f"{waiver['category']}: {waiver['reason']}"
        # Prepend so an existing non-strict xfail cannot weaken this waiver.
        by_nodeid[test].add_marker(
            pytest.mark.xfail(strict=True, reason=reason), append=False
        )


@pytest.hookimpl(hookwrapper=True, tryfirst=True)
def pytest_runtest_makereport(item, call):
    # Unwind after pytest's xfail wrapper so rejected failures stay failures.
    outcome = yield
    report = outcome.get_result()
    pattern = item.stash.get(WAIVER_PATTERN, None)
    if pattern is None:
        return
    if not report.skipped:
        return
    if not hasattr(report, "wasxfail"):
        reject_waiver(report, "A2A waiver expected a failure but the test skipped")
    elif not pattern.search(failure_text(call, report)):
        reject_waiver(
            report, f"A2A waiver failure did not match regex {pattern.pattern!r}"
        )


def failure_text(call, report):
    # Traceback source lines can contain the regex even when another error fired.
    if call.excinfo is not None:
        return str(call.excinfo.value)
    return report.longreprtext


def reject_waiver(report, message):
    report.longrepr = f"{message}\n{report.longreprtext}"
    report.outcome = "failed"
    if hasattr(report, "wasxfail"):
        del report.wasxfail
