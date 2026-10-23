## Skill improvement log

- 2026-09-15: With `cargo test -- --exact`, use the fully qualified Rust test name and verify that
  the run reports one executed test; a short-name filter can exit successfully after running zero
  tests and is not valid red-phase evidence.
