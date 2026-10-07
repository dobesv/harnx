"""Exercise the plugin through pytest's collection and reporting hooks."""

import json
from pathlib import Path

import pytest

pytest_plugins = ["pytester"]


@pytest.fixture
def run_suite(pytester, monkeypatch):
    pytester.makepyfile(
        a2a_waivers=Path(__file__).with_name("a2a_waivers.py").read_text()
    )
    monkeypatch.setenv("PYTHONPATH", str(pytester.path))
    monkeypatch.setenv("PYTEST_DISABLE_PLUGIN_AUTOLOAD", "1")
    monkeypatch.delenv("PYTEST_ADDOPTS", raising=False)

    def run(source, waivers, args=()):
        pytester.makepyfile(test_sample=source)
        pytester.makefile(".toml", waivers=waivers)
        return pytester.runpytest_subprocess(
            "-p", "a2a_waivers", "-rx", *args, timeout=30
        )

    return run


def waiver(test="test_sample.py::test_case", match="known failure"):
    return (
        f"[[waiver]]\ntest = {json.dumps(test)}\nmatch = {json.dumps(match)}\n"
        'category = "tck-defect"\nreason = "Known fixture defect."\n'
    )


def test_matching_failure(run_suite):
    result = run_suite(
        "def test_case():\n    assert False, 'known failure'\n", waiver()
    )
    result.assert_outcomes(xfailed=1)
    assert "tck-defect: Known fixture defect." in result.stdout.str()


def test_nonmatching_failure(run_suite):
    result = run_suite(
        "def test_case():\n    raise RuntimeError('new bug')\n", waiver()
    )
    result.assert_outcomes(failed=1)
    assert "A2A waiver failure did not match regex" in result.stdout.str()
    assert "RuntimeError: new bug" in result.stdout.str()


def test_traceback_source_cannot_match_unrelated_failure(run_suite):
    source = "def test_case():\n    raise RuntimeError('new bug')  # known failure\n"
    result = run_suite(source, waiver())
    result.assert_outcomes(failed=1)
    assert "RuntimeError: new bug" in result.stdout.str()


def test_skip(run_suite):
    source = "import pytest\ndef test_case():\n    pytest.skip('unavailable')\n"
    result = run_suite(source, waiver())
    result.assert_outcomes(failed=1)
    assert "A2A waiver expected a failure but the test skipped" in result.stdout.str()


@pytest.mark.parametrize(
    "source",
    [
        "import pytest\n@pytest.mark.skip(reason='unavailable')\ndef test_case():\n    pass\n",
        "import pytest\n@pytest.fixture(autouse=True)\ndef setup():\n    pytest.skip('unavailable')\ndef test_case():\n    pass\n",
        "import pytest\n@pytest.fixture(autouse=True)\ndef setup():\n    yield\n    pytest.skip('unavailable')\ndef test_case():\n    assert False, 'known failure'\n",
    ],
)
def test_skip_in_other_phases(run_suite, source):
    result = run_suite(source, waiver())
    assert result.ret == pytest.ExitCode.TESTS_FAILED
    assert "A2A waiver expected a failure but the test skipped" in result.stdout.str()


def test_pass_is_strict_xpass(run_suite):
    result = run_suite("def test_case():\n    pass\n", waiver())
    result.assert_outcomes(failed=1)
    assert "[XPASS(strict)] tck-defect: Known fixture defect." in result.stdout.str()


def test_existing_nonstrict_marker_does_not_weaken_waiver(run_suite):
    source = (
        "import pytest\n@pytest.mark.xfail(strict=False)\n"
        "def test_case():\n    assert True\n"
    )
    result = run_suite(source, waiver())
    result.assert_outcomes(failed=1)
    assert "[XPASS(strict)] tck-defect: Known fixture defect." in result.stdout.str()


def test_existing_xfail_cannot_hide_unrelated_failure(run_suite):
    source = (
        "import pytest\n@pytest.mark.xfail(strict=False)\n"
        "def test_case():\n    raise RuntimeError('new bug')\n"
    )
    result = run_suite(source, waiver())
    result.assert_outcomes(failed=1)
    assert "RuntimeError: new bug" in result.stdout.str()


def test_stale_waiver(run_suite):
    result = run_suite(
        "def test_case():\n    pass\n", waiver("test_sample.py::missing")
    )
    assert result.ret == pytest.ExitCode.USAGE_ERROR
    assert (
        "A2A waiver matched no collected test: test_sample.py::missing"
        in result.stderr.str()
    )


def test_waiver_checked_before_deselection(run_suite):
    source = "def test_case():\n    assert False\ndef test_other():\n    pass\n"
    result = run_suite(source, waiver(), ("-k", "test_other"))
    result.assert_outcomes(passed=1, deselected=1)


def test_nodeid_match_is_exact(run_suite):
    source = "class TestCase:\n    def test_case(self):\n        assert False\n"
    result = run_suite(source, waiver())
    assert result.ret == pytest.ExitCode.USAGE_ERROR
    assert "matched no collected test" in result.stderr.str()


@pytest.mark.parametrize("waivers", ["", "waiver = []"])
def test_empty_waivers(run_suite, waivers):
    result = run_suite("def test_case():\n    pass\n", waivers)
    result.assert_outcomes(passed=1)


def test_unwaived_failure(run_suite):
    result = run_suite("def test_case():\n    assert False\n", "")
    result.assert_outcomes(failed=1)


def test_unwaived_skip(run_suite):
    result = run_suite(
        "import pytest\ndef test_case():\n    pytest.skip('unavailable')\n", ""
    )
    result.assert_outcomes(skipped=1)


@pytest.mark.parametrize(
    ("waivers", "message"),
    [
        (waiver() * 2, "duplicate test"),
        (waiver().replace('"tck-defect"', '"unknown"'), "invalid category"),
        (waiver().replace('"Known fixture defect."', '""'), "missing or empty reason"),
        (waiver().replace('match = "known failure"\n', ""), "missing or empty match"),
        (waiver(match=""), "missing or empty match"),
        (waiver(match="["), "invalid match regex for test_sample.py::test_case"),
        ("[[waiver]\n", "cannot read"),
        ('waiver = "wrong"\n', "expected [[waiver]] entries"),
    ],
)
def test_invalid_waivers(run_suite, waivers, message):
    result = run_suite("def test_case():\n    pass\n", waivers)
    assert result.ret == pytest.ExitCode.USAGE_ERROR
    assert message in result.stderr.str()
