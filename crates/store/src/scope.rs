//! Tenant scope as a value, so every `Store` read carries who is asking.
//!
//! D13 (see `docs/DECISIONS.md`) fixed cross-tenant trace reads by putting the project filter *in
//! the query*: a foreign id is simply not found, so a 404 never confirms that someone else's row
//! exists. [`Scope`] generalises that to the whole trait — a point read that takes a bare id and is
//! authorised afterwards is an existence oracle no matter how careful the handler is, and the
//! handler is where the compensating `forbidden(...)` used to live.
//!
//! Two values only: a project key sees exactly its own rows ([`Scope::Project`]); an operator
//! (admin/dev key, background sweeps, the runner) sees everything ([`Scope::Operator`]). Rows whose
//! `project_id` is `NULL` are operator/legacy rows: `Operator` sees them, a project scope does not.

/// Who is asking. Constructed by the API from the request principal, and by background sweeps as
/// [`Scope::Operator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// A single tenant. Reads see only rows whose `project_id` equals this.
    Project(&'a str),
    /// The operator: every project, plus the `NULL`-project rows no tenant owns.
    Operator,
}

impl<'a> Scope<'a> {
    /// The project this scope is confined to, if any. `None` for [`Scope::Operator`].
    pub fn project(&self) -> Option<&'a str> {
        match self {
            Scope::Project(p) => Some(p),
            Scope::Operator => None,
        }
    }

    /// Whether a row carrying `project_id` is visible in this scope. `None` means the row has no
    /// project (operator/legacy) — only [`Scope::Operator`] sees those. Backends that cannot push
    /// the predicate into the query (a Firestore point read by document id) filter with this.
    pub fn allows(&self, project_id: Option<&str>) -> bool {
        match self {
            Scope::Operator => true,
            Scope::Project(p) => project_id == Some(*p),
        }
    }

    /// A **sargable** scope predicate, mirroring `sqlite::project_pred`: a concrete project is an
    /// index-seekable equality, while the operator arm is a constant TRUE that still consumes the
    /// same placeholder (bound to `NULL`), so callers bind exactly one parameter in both arms.
    ///
    /// `placeholder` is the backend's own form for the slot — `"?3"` for SQLite, `"$3"` for
    /// Postgres. Returns the predicate text and the value to bind into that slot.
    pub fn sql_pred(&self, col: &str, placeholder: &str) -> (String, Option<&'a str>) {
        match self {
            Scope::Project(p) => (format!("{col} = {placeholder}"), Some(*p)),
            Scope::Operator => (format!("{placeholder} IS NULL"), None),
        }
    }
}

/// The migration window: `None` meant "all projects" everywhere before this existed.
impl<'a> From<Option<&'a str>> for Scope<'a> {
    fn from(p: Option<&'a str>) -> Self {
        match p {
            Some(p) => Scope::Project(p),
            None => Scope::Operator,
        }
    }
}

impl<'a> From<&'a str> for Scope<'a> {
    fn from(p: &'a str) -> Self {
        Scope::Project(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_sees_null_project_rows_and_a_tenant_does_not() {
        assert!(Scope::Operator.allows(None));
        assert!(Scope::Operator.allows(Some("p1")));
        assert!(!Scope::Project("p1").allows(None));
        assert!(Scope::Project("p1").allows(Some("p1")));
        assert!(!Scope::Project("p1").allows(Some("p2")));
    }

    #[test]
    fn predicate_is_an_equality_for_a_tenant_and_a_bound_null_for_the_operator() {
        let (sql, bind) = Scope::Project("p1").sql_pred("project_id", "?2");
        assert_eq!(sql, "project_id = ?2");
        assert_eq!(bind, Some("p1"));
        let (sql, bind) = Scope::Operator.sql_pred("project_id", "$2");
        assert_eq!(sql, "$2 IS NULL");
        assert_eq!(bind, None);
    }

    #[test]
    fn option_round_trips_through_the_migration_conversion() {
        assert_eq!(Scope::from(Some("p1")), Scope::Project("p1"));
        assert_eq!(Scope::from(None), Scope::Operator);
    }
}
