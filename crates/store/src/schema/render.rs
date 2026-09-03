//! Shared rendering helpers: the type map, defaults, comment wrapping.
//!
//! Kept apart from the three dialect renderers so a type decision (`Ts` is a fixed-width string
//! everywhere, `Json` is text everywhere) is made in exactly one place and can be read as a
//! decision rather than inferred from three files that happen to agree.

use super::model::{Column, Dialect, Kind};

/// The physical type for one logical kind, per dialect.
///
/// BigQuery is the interesting column. `Ts → STRING`, not `TIMESTAMP`: the store's codec writes
/// fixed-width `RFC3339(Nanos, Z)` and every range filter in the product is a *string* comparison
/// over that format (see `crate::codec`), so a native `TIMESTAMP` would silently give BigQuery
/// different ordering and different boundary semantics from the two backends the tests run against.
/// `Json → STRING` for the same reason: the app writes serialized text, including the empty string
/// a legacy row can carry, and BigQuery's `JSON` type rejects that on load.
pub fn sql_type(kind: Kind, d: Dialect) -> &'static str {
    match (kind, d) {
        (Kind::Text | Kind::Ts | Kind::Json, Dialect::Sqlite | Dialect::Postgres) => "TEXT",
        (Kind::Text | Kind::Ts | Kind::Json, Dialect::BigQuery) => "STRING",
        (Kind::Int | Kind::Int32, Dialect::Sqlite) => "INTEGER",
        (Kind::Int, Dialect::Postgres) => "BIGINT",
        (Kind::Int32, Dialect::Postgres) => "INTEGER",
        (Kind::Int | Kind::Int32, Dialect::BigQuery) => "INT64",
        (Kind::Real, Dialect::Sqlite) => "REAL",
        (Kind::Real, Dialect::Postgres) => "DOUBLE PRECISION",
        (Kind::Real, Dialect::BigQuery) => "FLOAT64",
        (Kind::Bool, Dialect::Sqlite) => "INTEGER",
        (Kind::Bool, Dialect::Postgres) => "BOOLEAN",
        (Kind::Bool, Dialect::BigQuery) => "BOOL",
    }
}

/// The default literal, translated. Only booleans differ: the model spells every default in SQLite
/// terms (`0`/`1`), and Postgres wants `FALSE`/`TRUE` for a real `BOOLEAN`.
pub fn sql_default(c: &Column, d: Dialect) -> Option<String> {
    let raw = c.default?;
    Some(match (c.kind, d) {
        (Kind::Bool, Dialect::Postgres | Dialect::BigQuery) => match raw {
            "0" => "FALSE".to_string(),
            "1" => "TRUE".to_string(),
            other => other.to_string(),
        },
        _ => raw.to_string(),
    })
}

/// The column's full DDL fragment, without the trailing comma.
pub fn column_ddl(c: &Column, d: Dialect) -> String {
    let name = match d {
        Dialect::Postgres => c.pg_name(),
        _ => c.name.to_string(),
    };
    let inline_pk = c.pk && d != Dialect::BigQuery;
    let mut s = format!("{name} {}", sql_type(c.kind, d));
    // `PRIMARY KEY` already implies non-null, and spelling both out is not a no-op in SQLite: a
    // bare `TEXT PRIMARY KEY` column reports `notnull = 0` from `PRAGMA table_info`, so adding the
    // keyword would make every freshly-created table differ from every existing one.
    if !c.nullable && !inline_pk {
        s.push_str(" NOT NULL");
    }
    if let Some(def) = sql_default(c, d) {
        s.push_str(&format!(" DEFAULT {def}"));
    }
    if inline_pk {
        s.push_str(" PRIMARY KEY");
    }
    if let Some(r) = c.refs {
        let carried = match d {
            Dialect::Sqlite => true,
            Dialect::Postgres => c.refs_pg,
            Dialect::BigQuery => false,
        };
        if carried {
            s.push_str(&format!(" REFERENCES {r}"));
        }
    }
    s
}

/// One `ALTER TABLE … ADD COLUMN` for a post-ship column.
///
/// SQLite has no `IF NOT EXISTS` here — the applier tolerates "duplicate column name" instead,
/// which is the same idempotency by a different route.
pub fn add_column_stmt(table: &str, c: &Column, d: Dialect) -> String {
    let name = match d {
        Dialect::Postgres => c.pg_name(),
        _ => c.name.to_string(),
    };
    let mut s = match d {
        Dialect::Postgres => format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {name}"),
        _ => format!("ALTER TABLE {table} ADD COLUMN {name}"),
    };
    s.push(' ');
    s.push_str(sql_type(c.kind, d));
    if !c.nullable {
        s.push_str(" NOT NULL");
    }
    if let Some(def) = sql_default(c, d) {
        s.push_str(&format!(" DEFAULT {def}"));
    }
    s
}

/// Wrap `doc` as `--` SQL comments at `indent`, hard-wrapped near 96 columns so a generated file
/// reads like the hand-written one it replaces.
pub fn comment(doc: &str, indent: &str) -> String {
    if doc.is_empty() {
        return String::new();
    }
    let width = 96usize.saturating_sub(indent.len() + 3).max(40);
    let mut out = String::new();
    let mut line = String::new();
    for word in doc.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            out.push_str(&format!("{indent}-- {line}\n"));
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push_str(&format!("{indent}-- {line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tables;

    #[test]
    fn a_timestamp_is_a_fixed_width_string_in_every_dialect() {
        for d in Dialect::ALL {
            assert!(
                matches!(sql_type(Kind::Ts, *d), "TEXT" | "STRING"),
                "{d:?} must not use a native timestamp type — see crate::codec"
            );
        }
    }

    #[test]
    fn a_boolean_default_is_translated_for_postgres() {
        let c = tables::SCHEDULES.column("enabled").expect("enabled");
        assert_eq!(sql_default(c, Dialect::Sqlite).as_deref(), Some("1"));
        assert_eq!(sql_default(c, Dialect::Postgres).as_deref(), Some("TRUE"));
    }

    #[test]
    fn a_reserved_word_is_quoted_only_for_postgres() {
        let c = tables::LIMIT_RULES.column("window").expect("window");
        assert!(column_ddl(c, Dialect::Postgres).starts_with("\"window\""));
        assert!(column_ddl(c, Dialect::Sqlite).starts_with("window "));
    }
}
