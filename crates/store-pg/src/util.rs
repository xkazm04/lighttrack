//! Postgres-specific store helper: sqlx error mapping. The timestamp/enum/JSON codecs are shared
//! across all backends and re-exported here so the per-domain modules import them alongside `pgerr`
//! from one place — see [`lighttrack_store::codec`].

use lighttrack_store::StoreError;

pub(crate) use lighttrack_store::codec::{
    enum_to_str, fmt_ts, json_or_null, parse_enum, parse_ts, val_or_null,
};

pub(crate) fn pgerr(e: sqlx::Error) -> StoreError {
    StoreError::Other(format!("postgres: {e}"))
}

/// Split a `SELECT` list at its *top-level* commas — not the ones inside a call like
/// `COALESCE(received_at, ts)` — and reduce each entry to the name the row reader sees (its `AS`
/// alias when it has one). Test-only: it exists so a module can assert that its `COLS` constant
/// still lines up with the positional `try_get` indices its `from_row` uses.
#[cfg(test)]
pub(crate) fn select_list_names(cols: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in cols.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.iter()
        .map(|e| {
            let e = e.trim();
            match e.rsplit_once(" AS ") {
                Some((_, alias)) => alias.trim().to_string(),
                None => e.to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every store error from this backend must carry the `postgres:` marker, so an operator reading
    /// a 500 knows which backend produced it.
    #[test]
    fn pgerr_labels_the_backend() {
        match pgerr(sqlx::Error::RowNotFound) {
            StoreError::Other(msg) => assert!(msg.starts_with("postgres: "), "{msg}"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn select_list_names_ignores_commas_inside_calls() {
        let names = select_list_names("id, ts, COALESCE(received_at, ts) AS received_at, name");
        assert_eq!(names, vec!["id", "ts", "received_at", "name"]);
    }

    #[test]
    fn select_list_names_handles_nesting_and_quoted_identifiers() {
        let names = select_list_names(
            "a, COALESCE(NULLIF(substr(x, 1, 3), ''), 0) AS b, \"max\", COUNT(*) AS c",
        );
        assert_eq!(names, vec!["a", "b", "\"max\"", "c"]);
    }
}
