# The first `exec` fixture — Python + pytest

The smallest thing that answers *did the model's code actually work* (see
[`docs/BENCHMARK_FRAMEWORK.md` §3d](../../../docs/BENCHMARK_FRAMEWORK.md)).

| file | what it is |
| --- | --- |
| `Dockerfile` | the sandbox image: Python 3.12 + pytest, harness baked in, nothing installed at case time |
| `test_solution.py` | the grading harness, baked into the image so a candidate cannot read ahead of or edit it |
| `rubric.json` | a mixed rubric: one gating `exec` dimension (weight 3, floor 1.0) + one `llm` style dimension (weight 1) |

## The task, and why this one

`normalize_spaces(s)` — collapse runs of whitespace, strip the ends, raise `TypeError` on non-strings,
**and leave U+00A0 alone**.

That last clause is the whole point. The obvious answer, `" ".join(s.split())`, is what a weak model
reaches for, and it is wrong: `"\xa0".isspace()` is `True` in Python 3, so `str.split()` eats the
non-breaking space and the function silently corrupts text. A pass/fail dimension is only worth its
wall clock if the failure modes sit in the edges rather than the happy path.

Measured on CPython 3.14, 2026-09-05:

| candidate | result |
| --- | --- |
| reference (`re.sub` over ASCII whitespace only) | **7 passed** |
| naive (`" ".join(s.split())`) | **5 passed, 2 failed** — `test_non_breaking_space_is_preserved`, `test_rejects_non_string_input` |

So the dimension discriminates, and it discriminates for a reason a reviewer can state.

## Why Python and not Rust

§3d's example shows a `cargo test` dimension, and this fixture deliberately is not that. Open
question #2 in §3d is cold-start latency — exec time lands in the run report's p50/p95 columns as
**our** latency, not the model's — and a cold `cargo` build would swamp the first measurement of it.
Measure the floor with a fast harness; then decide what a slow one costs.

## Running it

Prerequisites: the `contree` CLI (`uv tool install contree-cli`) and a Nebius project **with
Sandboxes enabled**. A token alone is not enough — see *Known blocker* below.

```sh
# 1. credentials (NEBIUS_API_KEY / NEBIUS_AI_PROJECT come from .env)
contree auth -y
contree auth ls              # status must not be "inactive"

# 2. build and tag the fixture image (server-side; the CLI uploads this directory)
contree build -t lt-py-pytest:v1 fixtures/exec/python-pytest
contree images

# 3. sanity-check the image on its own — the placeholder solution must FAIL
contree run --use tag:lt-py-pytest:v1 -D -- pytest -q

# 4. create the rubric, then benchmark with the sandbox enabled
lt rubrics create --project <pid> --file fixtures/exec/python-pytest/rubric.json
lt-runner --sandbox bench --project <pid> --rubric-id <id> ...
```

Once `contree images` shows a UUID for the tag, replace `"image": "tag:lt-py-pytest:v1"` in
`rubric.json` with that UUID: a tag is mutable and stamps every verdict `best-effort`, a UUID stamps
it `exact`.

## Known blocker (2026-09-05)

`contree auth` accepts the token and saves the profile, then warns:

```
Warning: token is valid but sandboxes are disabled on default-project (no 'list' permission).
```

and `contree run` answers `HTTP 403: Insufficient permissions: spawn and list`. `contree images`
simply hung for over three minutes with no output.

Sandboxes is a beta service that must be enabled per project. Until it is, `lt-runner --sandbox`
refuses at startup by name, which is the designed behaviour rather than a workaround:

```
Error: the rubric has an `exec` dimension but the active `contree` profile is inactive: the token
is valid and Sandboxes is not enabled on project 'default-project'. Enable it for that project (or
point NEBIUS_AI_PROJECT at one where it is) and re-run `contree auth -y`.
```
