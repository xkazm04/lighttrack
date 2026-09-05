# The first `exec`-dimension fixture image (docs/BENCHMARK_FRAMEWORK.md §3d).
#
# Deliberately the narrowest thing that can answer the question "did the model's code work":
# Python, pytest, one task, no network, nothing to install at case time.
#
# Why not a Rust/cargo image, which is what §3d's example shows? Because §3d's open question #2 is
# cold-start latency — exec time lands in the run report's p50/p95 columns as *our* latency, not the
# model's — and `cargo test` on a cold target directory would swamp the very number we are trying to
# measure first. Measure the floor with a fast harness, then decide what a slow one costs.
#
# Build and tag (both are server-side; the CLI uploads this context):
#   contree build -t lt-py-pytest:v1 fixtures/exec/python-pytest
#   contree images
#
# Then pin the resulting UUID in the rubric rather than the tag: a tag is mutable and stamps the
# verdict `best-effort`, a UUID stamps it `exact`.

FROM python:3.12-slim

# Baked at build time, never at case time. A case that installs its own dependencies is measuring
# the network, and its latency number would be about PyPI.
RUN pip install --no-cache-dir pytest==8.3.3

WORKDIR /work

# The task's test suite ships in the image, so the candidate cannot edit, read ahead of, or delete
# the thing grading it. Only /work/solution.py is written per case.
COPY test_solution.py /work/test_solution.py

# A placeholder so the image is runnable on its own (`contree run -- pytest -q` fails cleanly rather
# than erroring on a missing import). Every real case overwrites it via `contree run --file`.
RUN printf 'def normalize_spaces(s):\n    raise NotImplementedError\n' > /work/solution.py
