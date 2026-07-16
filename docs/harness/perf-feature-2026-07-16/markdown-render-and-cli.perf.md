# Performance Optimizer — Markdown Render Layer & Operator CLI

> Total: 2
> Critical: 0 | High: 1 | Medium: 1 | Low: 0

## 1. `Table::render` allocates per-row/per-cell throwaway Strings and never pre-sizes the output buffer
- **Severity**: High
- **Category**: allocation
- **File**: `crates/render/src/md.rs:39-96`
- **Scenario**: A `list_traces`/`query_events`/`list_scores` render where the API returned a large result set. The CLI `--limit` is an unbounded `usize` (`crates/cli/src/main.rs:63,70` — default 20 but no upper clamp), and the runner client only caps the *body* at 128 MB (`crates/runner/src/http.rs:17`). A user passing `--limit 200000` interactively can drive a table with hundreds of thousands of rows through this one shared primitive.
- **Root cause**: `render()` starts `out = String::new()` with no capacity, then for every row calls `render_row(...)` which **allocates a fresh `String`** and is immediately `push_str`'d and dropped. `separator` likewise. Inside `render_row`, each cell calls `pad(...)`, which allocates *again* (see finding 2). So the work is O(rows×cols) short-lived heap allocations plus repeated geometric re-growth of the un-sized `out`.
- **Impact**: For an N-row × C-col table: ~N intermediate row Strings + ~N×C pad allocations, plus log₂-many `out` reallocations/copies as it grows. At 10⁵ rows this is millions of tiny alloc/free pairs and multiple full-buffer memcpys — the dominant cost of the render, all avoidable. At the common 20-row case it's negligible, which is why this is High not Critical.
- **Fix sketch**: Render straight into one buffer. Compute total width first (already done), `let mut out = String::with_capacity(est)` where `est ≈ (sum(width)+3*cols+1) * (rows+2)`, and change `render_row`/`separator`/`pad` to take `&mut String` and `push_str`/`push` in place instead of returning owned Strings. Zero per-row/per-cell allocation.
- **Trade-offs**: Threads a `&mut String` through three helpers; slightly less "functional" but same logic. None material.

## 2. `pad` makes two heap allocations per cell (`" ".repeat` + `format!`)
- **Severity**: Medium
- **Category**: allocation
- **File**: `crates/render/src/md.rs:86-96`
- **Scenario**: Every non-oversized cell of every rendered table (the common path — most cells are padded).
- **Root cause**: `pad` computes `let fill = " ".repeat(w - len)` (allocation #1) and then `format!("{s}{fill}")` / `format!("{fill}{s}")` (allocation #2, which copies both `s` and `fill`). The `repeat` result exists only to be concatenated and thrown away.
- **Impact**: 2 allocations + an extra copy for every padded cell — strictly wasted next to a direct write. On a 20-row table it's tens of allocations (immeasurable); it matters only because it multiplies finding 1's row count. Filed separately because it's a self-contained fix independent of the buffer-threading in #1.
- **Fix sketch**: Push directly into the target buffer: write `s`, then `for _ in 0..w-len { buf.push(' ') }` (or `buf.extend(std::iter::repeat(' ').take(pad))`) on the correct side per alignment. Uses `chars().count()` for `len` as today. Combined with #1 this makes table rendering fully in-place.
- **Trade-offs**: None material.

---

### Checked and deliberately NOT filed
- **CLI `Client::new()` per invocation** (`crates/cli/src/main.rs:422,358`): each `call`/`contribute` builds a fresh `reqwest::blocking::Client`. Not filed as perf — the `lt` CLI is one process = one command = one request, so there is no client to reuse and no hot path (the *reliability* gap that it lacks the runner's timeouts is out of scope here).
- **Runner startup** (`crates/runner/src/main.rs:35-43`): `dotenv()` + `Cli::parse()` + one shared `http::client()` built once and passed by reference everywhere; `EngineConfig` for `ScoreTraces` is rebuilt field-by-field specifically to avoid a clone. No repeated client/store construction — clean.
- **`Table` storing owned `Vec<Vec<String>>`**: eager owned cells are inherent to formatting numeric/glyph values; borrowing isn't feasible. Not a defect.
- **`md::render` two-pass width scan, `commafy`, `sparkline`, `parallel_map`**: all correctly single-pass / pre-sized / order-preserving. Nothing to optimize.
- **`serve.rs` job poll loop**: excluded per instructions (parallel audit).
