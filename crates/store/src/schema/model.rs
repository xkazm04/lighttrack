//! The declarative schema model: what a table, a column and an index *are*, independent of dialect.
//!
//! Nothing here renders SQL. The renderers ([`super::render_sqlite`], [`super::render_pg`],
//! [`super::render_bq`]) read these structures; [`super::tables`] is the data. The split is the
//! whole point of M14: a column is declared once, and every DDL file, migration list and select
//! list is a *view* of that declaration rather than a hand-kept copy of it.

use std::fmt::Write as _;

/// A backend's SQL dialect. Not a `Store` backend: Firestore has no DDL, and two backends could
/// share a dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dialect {
    Sqlite,
    Postgres,
    BigQuery,
}

impl Dialect {
    pub const ALL: &'static [Dialect] = &[Dialect::Sqlite, Dialect::Postgres, Dialect::BigQuery];

    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Sqlite => "sqlite",
            Dialect::Postgres => "postgres",
            Dialect::BigQuery => "bigquery",
        }
    }
}

/// The logical type of a column, mapped to a physical type per dialect by the renderers.
///
/// [`Kind::Ts`] and [`Kind::Json`] are logical, not physical: both are stored as text everywhere,
/// which is a *decision* (fixed-width RFC3339 so string range filters and `ORDER BY` are correct;
/// serialized JSON so no backend has to own a document type). Naming them keeps that decision
/// visible in the model instead of leaving sixty columns spelled `TEXT` with no way to tell which
/// of them carry a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Free text.
    Text,
    /// Fixed-width RFC3339(Nanos, Z). TEXT everywhere — see `crate::codec`.
    Ts,
    /// Serialized JSON. TEXT everywhere.
    Json,
    /// 64-bit integer (`INTEGER` / `BIGINT` / `INT64`).
    Int,
    /// 32-bit integer, where the Postgres schema shipped `INTEGER` rather than `BIGINT` and a
    /// widening would change what `sqlx` decodes into.
    Int32,
    /// Floating point (`REAL` / `DOUBLE PRECISION` / `FLOAT64`).
    Real,
    /// A truth value stored as Postgres `BOOLEAN`. The older boolean-ish columns are [`Kind::Int`]
    /// on purpose: Postgres shipped them as `BIGINT` and the app writes `bool as i64` into them.
    Bool,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Ts => "ts",
            Kind::Json => "json",
            Kind::Int => "int",
            Kind::Int32 => "int32",
            Kind::Real => "real",
            Kind::Bool => "bool",
        }
    }
}

/// One column, declared once.
///
/// `added_in` is the load-bearing field: a column with `Some(milestone)` shipped *after* the table
/// did, so it is rendered as an `ALTER TABLE … ADD COLUMN` rather than into the `CREATE TABLE` —
/// which is what makes a fresh database and a five-milestone-old one converge on the same shape.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub name: &'static str,
    pub kind: Kind,
    pub nullable: bool,
    /// Default in SQLite spelling (`0`, `1`, `'none'`, `1.0`). Booleans are translated per dialect.
    pub default: Option<&'static str>,
    /// Part of a single-column `PRIMARY KEY` declared inline.
    pub pk: bool,
    /// The milestone that added this column after the table shipped; `None` = original.
    pub added_in: Option<&'static str>,
    /// `REFERENCES other(col)` — rendered by SQLite always, by Postgres only when `refs_pg`.
    pub refs: Option<&'static str>,
    pub refs_pg: bool,
    /// A reserved word in Postgres: rendered as `"name"`, in DDL and in select lists.
    pub pg_quoted: bool,
    /// A select-list expression that replaces the bare column name (e.g. `COALESCE(received_at,
    /// ts) AS received_at`). DDL and INSERT lists always use [`Self::name`].
    pub select_as: Option<&'static str>,
    pub doc: &'static str,
}

impl Column {
    pub const fn new(name: &'static str, kind: Kind) -> Self {
        Self {
            name,
            kind,
            nullable: true,
            default: None,
            pk: false,
            added_in: None,
            refs: None,
            refs_pg: false,
            pg_quoted: false,
            select_as: None,
            doc: "",
        }
    }
    pub const fn nn(mut self) -> Self {
        self.nullable = false;
        self
    }
    pub const fn def(mut self, d: &'static str) -> Self {
        self.default = Some(d);
        self
    }
    pub const fn pk(mut self) -> Self {
        self.pk = true;
        self.nullable = false;
        self
    }
    pub const fn added(mut self, m: &'static str) -> Self {
        self.added_in = Some(m);
        self
    }
    pub const fn refs(mut self, t: &'static str) -> Self {
        self.refs = Some(t);
        self
    }
    /// A foreign key the Postgres schema also carries (only `prompt_versions.prompt_id` does).
    pub const fn refs_both(mut self, t: &'static str) -> Self {
        self.refs = Some(t);
        self.refs_pg = true;
        self
    }
    pub const fn quoted_pg(mut self) -> Self {
        self.pg_quoted = true;
        self
    }
    pub const fn select_as(mut self, e: &'static str) -> Self {
        self.select_as = Some(e);
        self
    }
    pub const fn doc(mut self, d: &'static str) -> Self {
        self.doc = d;
        self
    }

    /// The name as it appears in a Postgres statement.
    pub fn pg_name(&self) -> String {
        if self.pg_quoted {
            format!("\"{}\"", self.name)
        } else {
            self.name.to_string()
        }
    }
}

/// A secondary index. `dialects` exists because the checked-in DDLs do not agree today, and the
/// model's job is to make that disagreement visible rather than to silently create indexes on a
/// live database as a side effect of a refactor.
#[derive(Debug, Clone, Copy)]
pub struct Index {
    pub name: &'static str,
    pub columns: &'static str,
    pub unique: bool,
    pub predicate: Option<&'static str>,
    pub dialects: &'static [Dialect],
    /// Postgres column list, when it differs (an expression index).
    pub pg_columns: Option<&'static str>,
    pub doc: &'static str,
}

impl Index {
    pub const fn new(name: &'static str, columns: &'static str) -> Self {
        Self {
            name,
            columns,
            unique: false,
            predicate: None,
            dialects: &[Dialect::Sqlite, Dialect::Postgres],
            pg_columns: None,
            doc: "",
        }
    }
    pub const fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
    pub const fn predicate(mut self, p: &'static str) -> Self {
        self.predicate = Some(p);
        self
    }
    pub const fn only(mut self, d: &'static [Dialect]) -> Self {
        self.dialects = d;
        self
    }
    pub const fn pg_columns(mut self, c: &'static str) -> Self {
        self.pg_columns = Some(c);
        self
    }
    pub const fn doc(mut self, d: &'static str) -> Self {
        self.doc = d;
        self
    }
    pub fn serves(&self, d: Dialect) -> bool {
        self.dialects.contains(&d)
    }
}

/// A table, declared once.
#[derive(Debug, Clone, Copy)]
pub struct Table {
    pub name: &'static str,
    pub doc: &'static str,
    pub columns: &'static [Column],
    /// Composite primary key. Empty when a single column carries `.pk()`.
    pub primary_key: &'static [&'static str],
    /// `UNIQUE (…)` table constraints, as written.
    pub unique: &'static [&'static str],
    pub indexes: &'static [Index],
    /// BigQuery `PARTITION BY` / `CLUSTER BY`, when the table is worth partitioning.
    pub bq_partition: Option<&'static str>,
    pub bq_cluster: Option<&'static str>,
}

impl Table {
    pub const fn new(name: &'static str, columns: &'static [Column]) -> Self {
        Self {
            name,
            doc: "",
            columns,
            primary_key: &[],
            unique: &[],
            indexes: &[],
            bq_partition: None,
            bq_cluster: None,
        }
    }
    pub const fn doc(mut self, d: &'static str) -> Self {
        self.doc = d;
        self
    }
    pub const fn pk(mut self, cols: &'static [&'static str]) -> Self {
        self.primary_key = cols;
        self
    }
    pub const fn unique(mut self, u: &'static [&'static str]) -> Self {
        self.unique = u;
        self
    }
    pub const fn indexes(mut self, i: &'static [Index]) -> Self {
        self.indexes = i;
        self
    }
    pub const fn bq(mut self, partition: &'static str, cluster: &'static str) -> Self {
        self.bq_partition = Some(partition);
        self.bq_cluster = Some(cluster);
        self
    }

    /// Columns that shipped with the table — everything the `CREATE TABLE` declares.
    pub fn base_columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| c.added_in.is_none())
    }

    /// Columns added after the table shipped, in declaration order — which is also a valid
    /// migration order.
    pub fn added_columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| c.added_in.is_some())
    }

    /// The canonical select list: every column, in declared (wire) order, with each column's
    /// `select_as` expression where it has one.
    ///
    /// Declared order is deliberately NOT the physical order: a column added by `ALTER` lands at
    /// the end of a table on one database and inline on a freshly-created one, so a mapper reading
    /// by physical position would read a different column depending on the database's age. Naming
    /// every column in one stable order is what makes the positional mappers safe.
    pub fn select_list(&self, d: Dialect) -> String {
        let mut out = String::new();
        for (i, c) in self.columns.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            match (c.select_as, d) {
                (Some(e), _) => out.push_str(e),
                (None, Dialect::Postgres) => out.push_str(&c.pg_name()),
                (None, _) => out.push_str(c.name),
            }
        }
        out
    }

    /// `INSERT INTO <table> (…) VALUES (…)` with dialect-appropriate placeholders, covering every
    /// declared column in wire order — so a column added to the model that a writer forgets to
    /// bind is an arity error rather than a silently-NULL row.
    pub fn insert_stmt(&self, d: Dialect) -> String {
        let names: Vec<String> = self
            .columns
            .iter()
            .map(|c| match d {
                Dialect::Postgres => c.pg_name(),
                _ => c.name.to_string(),
            })
            .collect();
        let mut ph = String::new();
        for i in 1..=self.columns.len() {
            if i > 1 {
                ph.push(',');
            }
            let _ = match d {
                Dialect::Postgres => write!(ph, "${i}"),
                _ => write!(ph, "?{i}"),
            };
        }
        format!(
            "INSERT INTO {} ({}) VALUES ({ph})",
            self.name,
            names.join(", ")
        )
    }

    /// The column named, if this table declares it.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}
