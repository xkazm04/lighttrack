# Follow-ups — perf+feature campaign 2026-07-16

## Deferred from Wave 1: score-recording #1 (Critical) — server-side unscored-events query

**Why deferred:** the fix is cross-cutting (Store trait + SQLite + Postgres + Firestore + `/v1/scores` API +
schema index) and belongs to the backend-parity / query-correctness family (Wave 2/4), where it should be done
*with* conformance coverage rather than as a rushed change at the tail of the Wave-1 governor session.

**The bug** (`crates/runner/src/score.rs:59-84`): `score_once` builds its "already scored" set from a blind
`/v1/scores?limit=1000` (the API also clamps to 1000, `api/src/scores.rs:45`) and does the anti-join
client-side. Past 1000 stored scores, an event whose score aged out of that window is re-judged if the events
query still returns it — the idempotency guarantee silently degrades exactly when the table is large. Each
`--interval` tick also transfers up to 1000 full `Score` rows (incl. `reasoning` free-text) just to read
`event_id`, and re-does it every tick even with nothing new (on Firestore: up to 1000 billed doc reads/tick,
indefinitely).

**Recommended fix (two parts):**
1. **Index (also closes score-recording #2, High):** `CREATE INDEX IF NOT EXISTS idx_scores_event ON
   scores(event_id);` in both `schema/sqlite/001_init.sql` and `schema/postgres/001_init.sql`. Turns
   `list_trace_scores`' join into an index probe and backs the anti-join below.
2. **Server-side unscored query:** add `Store::list_unscored_events(project, limit)` (NOT a default impl —
   each backend implements it, and add it to `store/src/conformance.rs` so PG/Firestore can't silently inherit
   a wrong answer — see the store-trait conformance-gap finding).
   - SQLite/PG: `SELECT e.* FROM events e LEFT JOIN scores s ON s.event_id = e.id WHERE s.id IS NULL [AND
     e.project_id=?] ORDER BY e.ts DESC LIMIT N`.
   - Firestore (no server-side anti-join): fetch the events page, then read scores scoped to *exactly that
     page's event ids* (`event_id IN [...]`, batched by Firestore's IN-limit) and filter locally — correct and
     bounded, unlike the blind top-1000.
   - Expose via `/v1/events?unscored=1` (or a dedicated endpoint); switch `score_once` to it and delete the
     client-side `scored_ids` HashSet + the `/v1/scores?limit=1000` fetch.

**Test:** extend `store/src/conformance.rs` to insert N events, score some, and assert `list_unscored_events`
returns exactly the unscored set (proves parity across all three backends and guards the 1000-cap regression).

## Notes for whoever resumes

- Branch `vibeman/perf-feature-2026-07-16` holds Wave 1 (4 commits), off `main`, not pushed.
- The same "1000-row cap silently breaks correctness" shape (INDEX theme T7) also appears in relay dead-letter
  lookup and the calibration drift-window — worth sweeping together.
- The score-recording anti-join fix pairs naturally with Wave 4's conformance-suite extension (INDEX theme T2):
  do the conformance harness first, then the unscored-events method lands with coverage for free.
