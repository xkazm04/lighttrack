//! Building a query string from optional flags.
//!
//! Every read verb has the same two ways to go wrong: joining the first parameter with `&` (which
//! makes it part of the path) and sending an omitted flag as `key=` (which the API reads as a real,
//! empty value — a project named "", a cursor that resolves to nothing). One builder, so neither is
//! decided again per verb.

use std::fmt::Display;

pub(crate) struct Query {
    path: String,
    sep: char,
}

impl Query {
    pub(crate) fn new(base: &str) -> Self {
        Query {
            path: base.to_string(),
            sep: '?',
        }
    }

    /// Append `key=value` when there is a value. An empty string is an omission: that is the shape
    /// a shell hands over for an unset variable.
    pub(crate) fn push(&mut self, key: &str, value: Option<&str>) {
        if let Some(v) = value.filter(|s| !s.is_empty()) {
            self.path
                .push_str(&format!("{}{key}={}", self.sep, encode(v)));
            self.sep = '&';
        }
    }

    /// The same for a value that is already its own literal — a number or a bool, neither of which
    /// needs encoding.
    pub(crate) fn push_raw<T: Display>(&mut self, key: &str, value: Option<T>) {
        if let Some(v) = value {
            self.path.push_str(&format!("{}{key}={v}", self.sep));
            self.sep = '&';
        }
    }

    pub(crate) fn finish(self) -> String {
        self.path
    }
}

/// Percent-encode everything but the unreserved set. A cursor is opaque base64 and a judge is
/// `provider/model`, so neither can be pasted into a query string raw.
pub(crate) fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The separator order is the whole point: the first parameter opens the query string and every
    /// later one joins it, whichever of them happened to be given.
    #[test]
    fn the_first_parameter_present_opens_the_query_string() {
        let mut q = Query::new("/v1/rollup");
        q.push("project", None);
        q.push("by", Some("customer"));
        q.push_raw("limit", Some(5));
        assert_eq!(q.finish(), "/v1/rollup?by=customer&limit=5");
    }

    /// An omitted flag must send nothing at all — `?project=` is a project named "", which matches
    /// no rows and reads as "you have no data".
    #[test]
    fn an_omitted_or_empty_value_is_not_sent() {
        let mut q = Query::new("/v1/costs");
        q.push("project", None);
        q.push("since", Some(""));
        q.push_raw::<i64>("limit", None);
        assert_eq!(q.finish(), "/v1/costs");
    }

    #[test]
    fn values_are_percent_encoded() {
        let mut q = Query::new("/v1/labels");
        q.push("cursor", Some("a+b/c=="));
        assert_eq!(q.finish(), "/v1/labels?cursor=a%2Bb%2Fc%3D%3D");
        assert_eq!(encode("anthropic/haiku"), "anthropic%2Fhaiku");
    }
}
