//! Trace/span identity as a **type**, so the rule travels with the value.
//!
//! [`crate::ProviderId`] states the principle this module extends, in the host crate's own words:
//! *canonicalization happens at the seam so no downstream code has to remember to do it.* Trace ids
//! had the rule but not the type — [`crate::normalize_trace_ref`] was a free function the two
//! ingest doors called and every read door did not, so a W3C id in the spec's own uppercase 404'd
//! on a trace that existed.
//!
//! [`TraceRef`] canonicalizes on construction *and* on deserialize, which makes it usable in
//! `Path`/`Query` position: a read handler that takes a `TraceRef` cannot forget, and one that
//! takes a `String` is visibly the wrong type rather than the default one.

use serde::{Deserialize, Deserializer, Serialize};

/// A canonical trace/span reference.
///
/// A W3C/OTel id is hex and **case-insensitive** — `5B8E…` and `5b8e…` are the same trace — so hex
/// ids of the W3C lengths (32 for a trace, 16 for a span) are lower-cased. Anything else is a
/// caller's own opaque id (`"req-1"`, `"Order-7"`) and is preserved **verbatim**: case is meaningful
/// there, and folding it would merge distinct traces and mangle an id the operator reads back.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TraceRef(String);

impl TraceRef {
    /// Canonicalize an arbitrary string into a reference.
    pub fn new(id: &str) -> TraceRef {
        let is_w3c_hex = matches!(id.len(), 16 | 32) && id.chars().all(|c| c.is_ascii_hexdigit());
        if is_w3c_hex {
            TraceRef(id.to_ascii_lowercase())
        } else {
            TraceRef(id.to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The canonical spelling, owned — what the store layer's string filters take.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for TraceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TraceRef {
    fn from(s: &str) -> Self {
        TraceRef::new(s)
    }
}

impl From<String> for TraceRef {
    fn from(s: String) -> Self {
        TraceRef::new(&s)
    }
}

impl PartialEq<str> for TraceRef {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl<'de> Deserialize<'de> for TraceRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        // An **owned** `String`, deliberately not `Cow<'de, str>` the way `ProviderId` does it:
        // this type is used in `Path`/`Query` position, and the percent-decoding path deserializers
        // cannot always hand out a borrowed `&'de str`.
        let raw = String::deserialize(d)?;
        Ok(TraceRef::new(&raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The W3C lengths fold; everything else — including hex of the wrong length — is somebody's
    /// opaque id and is kept verbatim.
    #[test]
    fn w3c_hex_folds_and_opaque_ids_are_kept() {
        assert_eq!(
            TraceRef::new("5B8EFFF798038103D269B633813FC60C").as_str(),
            "5b8efff798038103d269b633813fc60c"
        );
        assert_eq!(
            TraceRef::new("EEE19B7EC3C1B174").as_str(),
            "eee19b7ec3c1b174"
        );
        assert_eq!(TraceRef::new("Order-7").as_str(), "Order-7");
        assert_eq!(
            TraceRef::new("ABCDEF0123456789A").as_str(),
            "ABCDEF0123456789A"
        );
        assert!(TraceRef::new("").is_empty());
    }

    /// Canonicalization on deserialize is the half that makes the type usable at a read door.
    #[test]
    fn deserialize_canonicalizes() {
        let r: TraceRef =
            serde_json::from_str("\"5B8EFFF798038103D269B633813FC60C\"").expect("parse");
        assert_eq!(r.as_str(), "5b8efff798038103d269b633813fc60c");
        // Round-trips as the canonical spelling, not the one it was given.
        assert_eq!(
            serde_json::to_string(&r).expect("serialize"),
            "\"5b8efff798038103d269b633813fc60c\""
        );
        let opaque: TraceRef = serde_json::from_str("\"Order-7\"").expect("parse");
        assert_eq!(opaque.as_str(), "Order-7");
    }
}
