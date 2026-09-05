"""The grading harness for the first `exec` fixture task.

It is baked into the image, not written per case, so the candidate can neither read it ahead of
time nor edit it. The command under test is `pytest -q`; its exit code is the whole verdict, and
nothing here prints a result for LightTrack to parse (see docs/BENCHMARK_FRAMEWORK.md §3d — a
harness that prints FAILED and exits 0 is a broken harness).

The task is small enough that a capable model gets it right and a weak one does not, with the
failure modes concentrated in the edge cases rather than the happy path. That is the only way a
pass/fail dimension carries information about the *model* rather than about the task.

Every non-ASCII character here is written as an escape on purpose. The discriminating case turns on
U+00A0, and a literal one in the source is invisible to a reader and one reformat away from
silently becoming a plain space — which would turn the test that does the work into a tautology.
"""

import pytest

from solution import normalize_spaces


def test_collapses_runs_of_spaces():
    assert normalize_spaces("a  b   c") == "a b c"


def test_strips_both_ends():
    assert normalize_spaces("  hello  ") == "hello"


def test_leaves_single_spaced_text_alone():
    assert normalize_spaces("already fine") == "already fine"


def test_empty_and_all_whitespace_become_empty():
    assert normalize_spaces("") == ""
    assert normalize_spaces("     ") == ""


def test_tabs_and_newlines_count_as_whitespace():
    assert normalize_spaces("a\tb\nc") == "a b c"
    assert normalize_spaces("a \t\n b") == "a b"


def test_non_breaking_space_is_preserved():
    """The edge that separates `" ".join(s.split())` from a correct answer.

    U+00A0 is not an ASCII space and must survive. `str.split()` with no argument splits on it in
    Python 3 (`"\\xa0".isspace()` is True), so the obvious one-liner returns "a b" here and fails.
    Verified 2026-09-05 on CPython 3.14. This is the case the dimension is really for.
    """
    assert normalize_spaces("a\xa0b") == "a\xa0b"


def test_rejects_non_string_input():
    with pytest.raises(TypeError):
        normalize_spaces(None)
