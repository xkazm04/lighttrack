//! One process-wide lock for tests that write the process environment.
//!
//! The environment is one block per process, and the test runner runs a binary's tests on parallel
//! threads. The standard library documents an environment write as sound on Unix only if no other
//! thread reads or writes the environment by any route; the 2024 edition marks the write `unsafe`
//! for that reason, and this workspace is on 2021, where the call carries no marker at all. A lock
//! declared inside one test module serializes the tests that take it and nothing else, so every test
//! in the workspace that writes the environment takes THIS lock, and
//! `crates/core/tests/env_write_census.rs` fails the build on a write anywhere else.
//!
//! What it does not close: a test that only reads, or a library reading through the C runtime
//! (name resolution, for one), takes no lock and can still overlap a write. That window exists only
//! off Windows; a test that needs it closed belongs in a process of its own.
//!
//! Test support only: compiled for this crate's own tests and behind the `test-env` feature, which
//! crates enable from `[dev-dependencies]`, so no shipped binary links it.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

/// Holds the lock for its lifetime and puts back every variable it touched when dropped, panics
/// included, so a test leaves the environment as it found it. One guard per test: a second `lock()`
/// on the same thread waits on itself.
pub struct EnvGuard {
    saved: Vec<(OsString, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    pub fn lock() -> Self {
        // A test that panicked while holding the lock restored its writes on unwind, so the poison
        // carries nothing worth failing the next test for.
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Self {
            saved: Vec::new(),
            _lock: lock,
        }
    }

    pub fn set(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.write(key.as_ref(), Some(value.as_ref()));
        self
    }

    pub fn remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.write(key.as_ref(), None);
        self
    }

    fn write(&mut self, key: &OsStr, value: Option<&OsStr>) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_owned(), std::env::var_os(key)));
        }
        apply(key, value);
    }
}

impl Drop for EnvGuard {
    // Runs before the fields drop, so the restore happens while the lock is still held.
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            apply(&key, value.as_deref());
        }
    }
}

/// The workspace's only environment writes, both reached with the lock held.
fn apply(key: &OsStr, value: Option<&OsStr>) {
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

#[cfg(test)]
mod tests {
    use super::EnvGuard;

    #[test]
    fn a_guard_holds_the_lock_and_puts_back_what_it_found() {
        let key = "LIGHTTRACK_TEST_ENV_GUARD";
        let mut env = EnvGuard::lock();
        assert!(
            super::LOCK.try_lock().is_err(),
            "a live guard holds the one lock"
        );
        env.set(key, "first").set(key, "second");
        assert_eq!(std::env::var(key).as_deref(), Ok("second"));
        drop(env);
        assert_eq!(
            std::env::var(key).ok(),
            None,
            "restored to unset, not to `first`"
        );

        // A failing test restores on unwind, and the poisoned lock does not fail the next one.
        let failed = std::panic::catch_unwind(|| {
            let mut env = EnvGuard::lock();
            env.set(key, "mid-test");
            panic!("the test failed while holding the guard");
        });
        assert!(failed.is_err());
        assert_eq!(std::env::var(key).ok(), None, "unwinding restored it");
        EnvGuard::lock().remove(key);
    }
}
