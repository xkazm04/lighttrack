//! `model_prices` collection (doc id = `<provider>__<model>`).

use serde_json::json;

use lighttrack_core::ModelPriceRow;
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn upsert_price(rest: &Rest, p: &ModelPriceRow) -> Result<()> {
    let id = format!("{}__{}", p.provider, p.model);
    let mut m = Fields::new();
    m.insert("provider".into(), json!(p.provider));
    m.insert("model".into(), json!(p.model));
    m.insert("input_per_mtok".into(), json!(p.input_per_mtok));
    m.insert("output_per_mtok".into(), json!(p.output_per_mtok));
    m.insert("cached_input_per_mtok".into(), json!(p.cached_input_per_mtok));
    m.insert("effective_date".into(), json!(fmt_ts(p.effective_date)));
    m.insert("source_url".into(), json!(p.source_url));
    rest.put_doc("model_prices", &id, &m)
}

pub(crate) fn list_prices(rest: &Rest) -> Result<Vec<ModelPriceRow>> {
    let docs = rest.query("model_prices", &[], None, None)?;
    let mut rows: Vec<ModelPriceRow> = docs.iter().map(price_from).collect::<Result<_>>()?;
    rows.sort_by(|a, b| (&a.provider, &a.model).cmp(&(&b.provider, &b.model)));
    Ok(rows)
}

fn price_from(m: &Fields) -> Result<ModelPriceRow> {
    Ok(ModelPriceRow {
        provider: freq(m, "provider")?,
        model: freq(m, "model")?,
        input_per_mtok: ff64(m, "input_per_mtok").unwrap_or(0.0),
        output_per_mtok: ff64(m, "output_per_mtok").unwrap_or(0.0),
        cached_input_per_mtok: ff64(m, "cached_input_per_mtok"),
        effective_date: parse_ts(&freq(m, "effective_date")?)?,
        source_url: fstr(m, "source_url"),
    })
}
