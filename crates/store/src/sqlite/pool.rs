//! A small pool of **read-only** SQLite connections.
//!
//! The store's writes stay serialized behind one connection mutex (admission control depends on it —
//! see [`super::SqliteStore::insert_event_checked`]). Reads don't: in WAL mode a reader takes a
//! consistent snapshot of the last committed state without blocking, or being blocked by, the
//! writer. So every read-only `Store` method borrows a connection from here instead of queueing
//! behind ingest.
//!
//! Two properties are load-bearing:
//!
//! * **Read-only at the SQLite level.** Pooled connections are opened `SQLITE_OPEN_READ_ONLY`, so a
//!   read path *cannot* mutate or interleave with the write connection's transaction even by
//!   mistake — the admission critical section stays exactly as atomic as it was.
//! * **WAL required.** Without WAL a reader holds a SHARED lock that blocks the writer's EXCLUSIVE
//!   lock, which would make a pool *worse* than the single mutex. The pool is therefore only ever
//!   built after WAL is confirmed engaged; otherwise it stays [`ReadPool::disabled`] and reads fall
//!   back to the write connection (exactly the previous behavior).

use std::ops::Deref;
use std::path::Path;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::Result;

/// Pooled read connections. Small on purpose: SQLite readers are cheap but not free, and the
/// workload this unblocks is "a handful of dashboard queries alongside ingest", not a read farm.
pub(super) const DEFAULT_SIZE: usize = 4;

/// How long a pooled reader waits on a busy database before giving up. WAL readers should never
/// block on the writer; this covers checkpoint/schema-change windows.
pub(super) const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Operator override for the pool size (`0` disables pooling entirely).
pub(super) fn configured_size() -> usize {
    match std::env::var("LIGHTTRACK_SQLITE_READ_POOL") {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(DEFAULT_SIZE).min(32),
        Err(_) => DEFAULT_SIZE,
    }
}

pub(super) struct ReadPool {
    idle: Mutex<Vec<Connection>>,
    free: Condvar,
    size: usize,
}

impl ReadPool {
    /// A pool that hands out nothing — callers fall back to the write connection.
    pub(super) fn disabled() -> Self {
        Self {
            idle: Mutex::new(Vec::new()),
            free: Condvar::new(),
            size: 0,
        }
    }

    /// Open `size` read-only connections against an already-migrated database file.
    pub(super) fn open(path: &Path, size: usize) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conns = Vec::with_capacity(size);
        for _ in 0..size {
            let c = Connection::open_with_flags(path, flags)?;
            c.busy_timeout(BUSY_TIMEOUT)?;
            conns.push(c);
        }
        Ok(Self {
            idle: Mutex::new(conns),
            free: Condvar::new(),
            size,
        })
    }

    #[cfg(test)]
    pub(super) fn size(&self) -> usize {
        self.size
    }

    /// Borrow a reader, blocking until one is free. `None` when the pool is disabled.
    pub(super) fn acquire(&self) -> Option<Pooled<'_>> {
        if self.size == 0 {
            return None;
        }
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(conn) = idle.pop() {
                return Some(Pooled {
                    pool: self,
                    conn: Some(conn),
                });
            }
            idle = self.free.wait(idle).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// A checked-out reader, returned to the pool on drop — including on unwind, so a panicking read
/// path can't permanently shrink the pool.
pub(super) struct Pooled<'a> {
    pool: &'a ReadPool,
    conn: Option<Connection>,
}

impl Deref for Pooled<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("pooled connection taken only in Drop")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut idle = self
                .pool
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            idle.push(conn);
            drop(idle);
            self.pool.free.notify_one();
        }
    }
}
