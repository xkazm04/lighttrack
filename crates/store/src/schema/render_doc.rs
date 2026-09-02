//! The generated table index in `docs/DATA_MODEL.md`.
//!
//! `DATA_MODEL.md` is prose, and should stay prose: it explains what a column *means*, which no
//! renderer can. What it could not do by hand is stay complete — it documents eight of the model's
//! twenty-five tables, and nothing said so. This renders the index only, between markers, so the
//! document always names every table even when only some of them have a section.

use super::model::Dialect;
use super::tables;

pub const BEGIN: &str =
    "<!-- BEGIN generated table index (cargo test -p lighttrack-store --test schema_doc) -->";
pub const END: &str = "<!-- END generated table index -->";

/// The index block, markers included.
pub fn table_index() -> String {
    let mut out = String::from(BEGIN);
    out.push_str("\n\n## Every table (generated)\n\n");
    out.push_str(
        "Rendered from the declarative model in `crates/store/src/schema/tables/`, which is also \
         what generates the three DDLs in `schema/`. A table here without a section above is one \
         this document has not explained yet — which is the point of generating the list.\n\n",
    );
    out.push_str("| Table | Columns | Added after ship | Indexes | Key |\n|---|---|---|---|---|\n");
    for t in tables::all() {
        let added = t.added_columns().count();
        let key: Vec<&str> = if t.primary_key.is_empty() {
            t.columns.iter().filter(|c| c.pk).map(|c| c.name).collect()
        } else {
            t.primary_key.to_vec()
        };
        let idx = t
            .indexes
            .iter()
            .filter(|i| i.serves(Dialect::Sqlite))
            .count();
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | `{}` |\n",
            t.name,
            t.columns.len(),
            added,
            idx,
            key.join(", ")
        ));
    }
    out.push_str(&format!(
        "\nTotals: **{} tables**, **{} columns**, of which **{}** were added after their table \
         shipped (those are `ALTER TABLE … ADD COLUMN` on every dialect, never edits to a `CREATE \
         TABLE`). Schema fingerprint: `{}` — the same value `GET /v1/capabilities` reports.\n\n",
        tables::all().len(),
        tables::all().iter().map(|t| t.columns.len()).sum::<usize>(),
        tables::all()
            .iter()
            .map(|t| t.added_columns().count())
            .sum::<usize>(),
        super::fingerprint(),
    ));
    out.push_str(END);
    out
}

/// Replace the marked block in `doc`, or append it when the markers are absent.
pub fn splice(doc: &str) -> String {
    let block = table_index();
    match (doc.find(BEGIN), doc.find(END)) {
        (Some(a), Some(b)) if b > a => {
            format!("{}{}{}", &doc[..a], block, &doc[b + END.len()..])
        }
        _ => format!("{}\n\n{}\n", doc.trim_end(), block),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_names_every_table_and_splices_idempotently() {
        let doc = "# Data model\n\nprose\n";
        let once = splice(doc);
        for t in tables::all() {
            assert!(once.contains(&format!("| `{}` |", t.name)), "{}", t.name);
        }
        assert_eq!(splice(&once), once, "splicing twice must not stack blocks");
        assert!(once.starts_with("# Data model"), "the prose is preserved");
    }
}
