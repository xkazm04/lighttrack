//! Tenancy verbs: projects and the API keys minted on them.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum ProjectsCmd {
    /// Create a project — the tenancy boundary every key, limit and event hangs off.
    Create {
        #[arg(long)]
        name: String,
        /// Choose the project id (1–64 chars: letter/digit first, then letters, digits, `-`, `_`,
        /// `.`). This is the id you put in `LIGHTTRACK_PROJECT` and in URLs. Omit it for a UUID.
        #[arg(long)]
        id: Option<String>,
        /// Payload persistence policy: `none` | `hash` | `drop`.
        #[arg(long)]
        redaction: Option<String>,
    },
    /// Every project on this deployment.
    List,
    /// Change a project's name, enablement or policy flags; an omitted flag is left as it was.
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        /// Re-enable a disabled project.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Stop this project's keys opening anything. The rows are kept.
        #[arg(long)]
        disable: bool,
        /// Payload persistence policy, enforced on the NEXT ingested event: `none` | `hash` | `drop`.
        #[arg(long)]
        redaction: Option<String>,
        /// Consent to publishing privacy-safe collective digests from this project.
        #[arg(long = "collective-opt-in")]
        collective_opt_in: Option<bool>,
        /// Refuse prompt promotion when the judge behind the evidence is not trusted.
        #[arg(long = "require-trusted-judge")]
        require_trusted_judge: Option<bool>,
    },
    /// Archive a project: disabled and stamped `archived_at`, with every stored row kept.
    Archive { id: String },
    /// What the ingest boundary actually did to this project's stored rows — counted from the rows.
    Redaction {
        id: String,
        /// RFC3339 lower bound on arrival time (default: 30 days back).
        #[arg(long)]
        since: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum KeysCmd {
    /// Mint an API key on a project. The secret is printed ONCE and is never retrievable again.
    Create {
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "default")]
        name: String,
        /// What the key may do: `ingest`, `read`, `manage`. Repeatable. Omitted ⇒ the server's
        /// back-compat default (`ingest` + `read`); a key shipped inside a client app should be
        /// `--scope ingest` so it cannot read the project's stored prompts back.
        #[arg(long = "scope")]
        scopes: Vec<String>,
        /// Hard expiry, RFC3339 (e.g. `2027-01-01T00:00:00Z`). Past it the key stops working.
        #[arg(long)]
        expires: Option<String>,
    },
    /// List a project's keys with their scopes, expiry, last use and revocation state.
    List {
        #[arg(long)]
        project: String,
    },
    /// Mint a successor with the same name and scopes, and give this key a deadline instead of
    /// killing it — so a fleet still holding the old secret has a window to redeploy.
    ///
    /// The successor's secret is printed ONCE, like `create`'s.
    Rotate {
        #[arg(long)]
        project: String,
        /// The key id to rotate (from `lt keys list`).
        id: String,
        /// How long the old key keeps working. `0` retires it at once.
        #[arg(long = "grace-secs")]
        grace_secs: Option<i64>,
    },
    /// Revoke a key immediately (soft — the row is kept for audit).
    Revoke {
        #[arg(long)]
        project: String,
        id: String,
    },
}
