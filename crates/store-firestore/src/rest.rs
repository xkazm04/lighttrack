//! Minimal Firestore REST client over blocking reqwest: document GET / upsert (PATCH) / partial
//! PATCH / `runQuery`. Returns decoded plain-field maps (see `codec`).

use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::Method;
use serde_json::{json, Value};

use lighttrack_store::{Result, StoreError};

use crate::codec::{decode_doc, encode_fields, encode_value, other, Fields};

pub(crate) struct Rest {
    client: Client,
    base: String, // https://.../v1/projects/<p>/databases/(default)/documents
    token: Option<String>,
}

impl Rest {
    pub(crate) fn new(base: String, token: Option<String>) -> Self {
        Self {
            client: Client::new(),
            base,
            token,
        }
    }

    fn req(&self, method: Method, url: String) -> RequestBuilder {
        let r = self.client.request(method, url);
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    /// GET a document's fields; `None` on 404.
    pub(crate) fn get_doc(&self, collection: &str, id: &str) -> Result<Option<Fields>> {
        let url = format!("{}/{}/{}", self.base, collection, id);
        let resp = self.req(Method::GET, url).send().map_err(re)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(decode_doc(&json_ok(resp)?)))
    }

    /// Create-or-replace a document by id (full PATCH).
    pub(crate) fn put_doc(&self, collection: &str, id: &str, fields: &Fields) -> Result<()> {
        let url = format!("{}/{}/{}", self.base, collection, id);
        let body = json!({ "fields": encode_fields(fields) });
        json_ok(self.req(Method::PATCH, url).json(&body).send().map_err(re)?).map(|_| ())
    }

    /// Create a document by id, failing with [`StoreError::Conflict`] if it already exists. Uses
    /// the `currentDocument.exists=false` precondition — [`Rest::put_doc`]'s plain PATCH is an
    /// upsert that would silently overwrite, which is data loss on an insert path.
    pub(crate) fn create_doc(&self, collection: &str, id: &str, fields: &Fields) -> Result<()> {
        let url = format!("{}/{}/{}?currentDocument.exists=false", self.base, collection, id);
        let body = json!({ "fields": encode_fields(fields) });
        let resp = self.req(Method::PATCH, url).json(&body).send().map_err(re)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let text = resp.text().map_err(re)?;
        if status.as_u16() == 409
            || text.contains("ALREADY_EXISTS")
            || text.contains("FAILED_PRECONDITION")
        {
            return Err(StoreError::Conflict(format!("'{collection}/{id}' already exists")));
        }
        Err(other(format!("firestore HTTP {}: {text}", status.as_u16())))
    }

    /// PATCH only the named fields (the rest of the doc is untouched).
    pub(crate) fn patch_fields(
        &self,
        collection: &str,
        id: &str,
        fields: &Fields,
        mask: &[&str],
    ) -> Result<()> {
        let q: Vec<String> = mask.iter().map(|m| format!("updateMask.fieldPaths={m}")).collect();
        let url = format!("{}/{}/{}?{}", self.base, collection, id, q.join("&"));
        let body = json!({ "fields": encode_fields(fields) });
        json_ok(self.req(Method::PATCH, url).json(&body).send().map_err(re)?).map(|_| ())
    }

    /// DELETE a document by id. Returns `true` when it existed, `false` when it didn't — the
    /// `exists=true` precondition turns Firestore's silently-idempotent delete into an observable
    /// outcome (the Store contract maps unknown-id deletes to `false` → API 404).
    pub(crate) fn delete_doc(&self, collection: &str, id: &str) -> Result<bool> {
        let url = format!("{}/{}/{}?currentDocument.exists=true", self.base, collection, id);
        let resp = self.req(Method::DELETE, url).send().map_err(re)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(true);
        }
        let text = resp.text().map_err(re)?;
        if status.as_u16() == 404
            || status.as_u16() == 409
            || text.contains("NOT_FOUND")
            || text.contains("FAILED_PRECONDITION")
        {
            return Ok(false);
        }
        Err(other(format!("firestore HTTP {}: {text}", status.as_u16())))
    }

    /// `runQuery` returning decoded field maps.
    pub(crate) fn query(
        &self,
        collection: &str,
        filters: &[(&str, &str, Value)],
        order: Option<(&str, bool)>,
        limit: Option<usize>,
    ) -> Result<Vec<Fields>> {
        Ok(self
            .query_raw(collection, filters, order, limit)?
            .iter()
            .map(decode_doc)
            .collect())
    }

    /// `runQuery` returning raw documents (with `name` + `updateTime`) — used by `claim_job`.
    pub(crate) fn query_raw(
        &self,
        collection: &str,
        filters: &[(&str, &str, Value)],
        order: Option<(&str, bool)>,
        limit: Option<usize>,
    ) -> Result<Vec<Value>> {
        let url = format!("{}:runQuery", self.base);
        let body = json!({ "structuredQuery": build_sq(collection, filters, order, limit) });
        let arr = json_ok(self.req(Method::POST, url).json(&body).send().map_err(re)?)?;
        let mut out = Vec::new();
        if let Some(items) = arr.as_array() {
            for it in items {
                if let Some(doc) = it.get("document") {
                    out.push(doc.clone());
                }
            }
        }
        Ok(out)
    }

    /// Non-transactional commit of one field update, optionally guarded by an `updateTime`
    /// precondition (optimistic concurrency). Returns `false` when the precondition fails (another
    /// writer changed the doc first) — the basis for a concurrency-safe `claim_job`.
    pub(crate) fn commit_update(
        &self,
        doc_name: &str,
        fields: &Fields,
        mask: &[&str],
        precond_update_time: Option<&str>,
    ) -> Result<bool> {
        let mut write = json!({
            "update": { "name": doc_name, "fields": encode_fields(fields) },
            "updateMask": { "fieldPaths": mask },
        });
        if let Some(ut) = precond_update_time {
            write["currentDocument"] = json!({ "updateTime": ut });
        }
        let url = format!("{}:commit", self.base);
        let resp = self
            .req(Method::POST, url)
            .json(&json!({ "writes": [write] }))
            .send()
            .map_err(re)?;
        let status = resp.status();
        let text = resp.text().map_err(re)?;
        if status.is_success() {
            return Ok(true);
        }
        if status.as_u16() == 409 || text.contains("FAILED_PRECONDITION") || text.contains("ABORTED") {
            return Ok(false);
        }
        Err(other(format!("firestore commit HTTP {}: {text}", status.as_u16())))
    }
}

/// Build a `structuredQuery`: AND of `(field, op, value)` filters; optional `(orderBy, desc)`; limit.
fn build_sq(
    collection: &str,
    filters: &[(&str, &str, Value)],
    order: Option<(&str, bool)>,
    limit: Option<usize>,
) -> Value {
    let mut sq = json!({ "from": [ { "collectionId": collection } ] });
    if !filters.is_empty() {
        let fs: Vec<Value> = filters
            .iter()
            .map(|(f, op, v)| {
                json!({ "fieldFilter": { "field": {"fieldPath": f}, "op": op, "value": encode_value(v) } })
            })
            .collect();
        sq["where"] = if fs.len() == 1 {
            fs.into_iter().next().unwrap()
        } else {
            json!({ "compositeFilter": { "op": "AND", "filters": fs } })
        };
    }
    if let Some((f, desc)) = order {
        sq["orderBy"] = json!([ {
            "field": { "fieldPath": f },
            "direction": if desc { "DESCENDING" } else { "ASCENDING" }
        } ]);
    }
    if let Some(n) = limit {
        sq["limit"] = json!(n as i64);
    }
    sq
}

fn re(e: reqwest::Error) -> StoreError {
    other(format!("firestore http: {e}"))
}

fn json_ok(resp: Response) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().map_err(re)?;
    if !status.is_success() {
        return Err(other(format!("firestore HTTP {}: {text}", status.as_u16())));
    }
    serde_json::from_str(&text).map_err(|e| other(format!("firestore bad json: {e}")))
}
