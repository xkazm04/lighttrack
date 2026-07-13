//! The `--gate` exit-code contract for `lt-runner bench`. A benchmark used as a CI gate must fail
//! the build when quality regresses; a plain run must not. This maps the honest run status (from the
//! significance verdict) to a process exit code so a pipeline step can branch on it.

/// A target regressed against its baseline → fail the build.
pub(crate) const EXIT_REGRESSED: i32 = 3;
/// No baseline (or no scored run) to gate against → distinct code, so CI can treat "unverified"
/// differently from a real regression (e.g. warn instead of hard-fail).
pub(crate) const EXIT_NO_BASELINE: i32 = 4;

/// Map a run's final status to a `--gate` exit code. `passed` (and anything else) → 0.
pub(crate) fn gate_exit_code(status: &str) -> i32 {
    match status {
        "regressed" => EXIT_REGRESSED,
        "no_baseline" => EXIT_NO_BASELINE,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_status_to_exit_code() {
        assert_eq!(gate_exit_code("regressed"), EXIT_REGRESSED);
        assert_eq!(gate_exit_code("no_baseline"), EXIT_NO_BASELINE);
        assert_eq!(gate_exit_code("passed"), 0);
        // Any unrecognized/legacy status is treated as non-blocking.
        assert_eq!(gate_exit_code("completed"), 0);
        assert_eq!(gate_exit_code(""), 0);
    }
}
