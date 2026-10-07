"""Exercise the plugin through pytest's collection and reporting hooks."""

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest


@pytest.fixture
def run_suite(tmp_path):
    shutil.copy2(Path(__file__).with_name("a2a_waivers.py"), tmp_path)
    env = os.environ.copy()
    env["PYTHONPATH"] = str(tmp_path)
    env["PYTEST_DISABLE_PLUGIN_AUTOLOAD"] = "1"
    env.pop("PYTEST_ADDOPTS", None)

    def run(source, waivers, args=()):
        (tmp_path / "test_sample.py").write_text(source)
        (tmp_path / "waivers.toml").write_text(waivers)
        return subprocess.run(
            [sys.executable, "-m", "pytest", "-p", "a2a_waivers", "-rx", *args],
            cwd=tmp_path,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    return run


def waiver(test="test_sample.py::test_case"):
    return (
        f"[[waiver]]\ntest = {json.dumps(test)}\n"
        'category = "tck-defect"\nreason = "Known fixture defect."\n'
    )


@pytest.mark.parametrize(
    ("body", "code", "summary"),
    [
        ("assert False", 0, "1 xfailed"),
        ("assert True", 1, "[XPASS(strict)] tck-defect: Known fixture defect."),
    ],
)
def test_strict_xfail(run_suite, body, code, summary):
    result = run_suite(f"def test_case():\n    {body}\n", waiver())
    assert result.returncode == code, result.stdout + result.stderr
    assert summary in result.stdout
    assert "tck-defect: Known fixture defect." in result.stdout


def test_existing_nonstrict_marker_does_not_weaken_waiver(run_suite):
    source = (
        "import pytest\n@pytest.mark.xfail(strict=False)\n"
        "def test_case():\n    assert True\n"
    )
    result = run_suite(source, waiver())
    assert result.returncode == 1, result.stdout + result.stderr
    assert "[XPASS(strict)] tck-defect: Known fixture defect." in result.stdout


def test_stale_waiver(run_suite):
    result = run_suite(
        "def test_case():\n    pass\n", waiver("test_sample.py::missing")
    )
    assert result.returncode == 4, result.stdout + result.stderr
    assert (
        "A2A waiver matched no collected test: test_sample.py::missing" in result.stderr
    )


def test_waiver_checked_before_deselection(run_suite):
    source = "def test_case():\n    assert False\ndef test_other():\n    pass\n"
    result = run_suite(source, waiver(), ("-k", "test_other"))
    assert result.returncode == 0, result.stdout + result.stderr
    assert "1 passed, 1 deselected" in result.stdout


def test_nodeid_match_is_exact(run_suite):
    source = "class TestCase:\n    def test_case(self):\n        assert False\n"
    result = run_suite(source, waiver())
    assert result.returncode == 4, result.stdout + result.stderr
    assert "matched no collected test" in result.stderr


@pytest.mark.parametrize("waivers", ["", "waiver = []"])
def test_empty_waivers(run_suite, waivers):
    result = run_suite("def test_case():\n    pass\n", waivers)
    assert result.returncode == 0, result.stdout + result.stderr
    assert "1 passed" in result.stdout


def test_unwaived_failure(run_suite):
    result = run_suite("def test_case():\n    assert False\n", "")
    assert result.returncode == 1, result.stdout + result.stderr
    assert "1 failed" in result.stdout


@pytest.mark.parametrize(
    ("waivers", "message"),
    [
        (waiver() * 2, "duplicate test"),
        (waiver().replace('"tck-defect"', '"unknown"'), "invalid category"),
        (waiver().replace('"Known fixture defect."', '""'), "missing or empty reason"),
        ("[[waiver]\n", "cannot read"),
        ('waiver = "wrong"\n', "expected [[waiver]] entries"),
    ],
)
def test_invalid_waivers(run_suite, waivers, message):
    result = run_suite("def test_case():\n    pass\n", waivers)
    assert result.returncode == 4, result.stdout + result.stderr
    assert message in result.stderr
