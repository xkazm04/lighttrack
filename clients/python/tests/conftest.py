"""Make the suite test the client *in this tree*, not whichever copy happens to be installed.

Without this, `python -m pytest clients/python/tests` from the repo root imports `lighttrack` from
site-packages if the package is installed anywhere on the machine — so the suite passes while
asserting nothing about the working tree, and a change to `clients/python/lighttrack/` is invisible
to its own tests until someone reinstalls. That is the worst shape a test suite can have: green, and
about the wrong code.

Prepending the package root to `sys.path` is enough, because `sys.path` order decides the winner and
`pytest`'s own rootdir insertion does not reach a package one directory up from the tests.
"""

import sys
from pathlib import Path

PACKAGE_ROOT = str(Path(__file__).resolve().parents[1])

if sys.path and sys.path[0] != PACKAGE_ROOT:
    # Drop any earlier copy of the same path so the entry we want is unambiguously first.
    sys.path = [PACKAGE_ROOT] + [p for p in sys.path if p != PACKAGE_ROOT]
