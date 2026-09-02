//! Currency normalization: ISO-4217 minor-unit handling + a config-seeded FX table to USD.
//!
//! Billing providers report an amount in the currency's **minor unit** (Stripe: `amount_paid`;
//! Polar: `total_amount`). Two things must happen before that number can be summed as USD revenue:
//!
//! 1. **Minor → major** by the *currency's* decimal places, not a blanket `/100`. Zero-decimal
//!    currencies (JPY, KRW, VND, …) have minor == major, and three-decimal ones (KWD, BHD, …) divide
//!    by 1000. Dividing JPY by 100 understates yen revenue 100×.
//! 2. **Major → USD** at a known rate. A EUR invoice is not $X at 1:1; it converts at the FX rate.
//!
//! Rates live in `config/fx_rates.json` (path overridable via `LIGHTTRACK_FX_RATES`), loaded once at
//! startup — the same "seed a static book from a JSON file" pattern as `pricing.json`. A currency
//! with **no rate** is deliberately *not* silently treated as USD: it is stored at 1:1 (so the money
//! is not lost) but marked *unconverted* ([`UsdAmount::converted`] = `false`), and the margin surface
//! reports which currencies were involved so the operator knows a rate is missing. See docs/CURRENCY.md.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The result of normalizing a provider minor-unit amount into USD.
#[derive(Debug, Clone, Copy)]
pub struct UsdAmount {
    /// The amount in USD. Genuine USD when `converted`; a 1:1 fallback (major units taken as USD) when
    /// the currency had no rate in the table.
    pub amount_usd: f64,
    /// Whether a real conversion happened: the currency is the base (USD) or had a rate. `false` means
    /// the 1:1 fallback was used and the figure is approximate — the caller should surface that.
    pub converted: bool,
    /// USD per one major unit of the source currency: `1.0` for the base, the book's rate for a
    /// convertible currency, and `None` on the 1:1 fallback.
    ///
    /// `None` rather than `Some(1.0)` for the fallback on purpose: "one unit really is worth a
    /// dollar" and "we had no rate and stored the number anyway" are different facts, and a stored
    /// `1.0` would make the second indistinguishable from the first forever after.
    pub rate: Option<f64>,
}

/// A static FX book: the reporting base (USD) plus per-currency rates expressed as the **USD value of
/// one unit** of the currency (so `amount_usd = major_units * rate`). Seeded from `config/fx_rates.json`.
#[derive(Debug, Clone)]
pub struct FxTable {
    base: String,
    /// Uppercase currency code → USD per one unit of that currency.
    rates: HashMap<String, f64>,
    /// A stable name for *this* book, stamped on every row it converts. See [`FxTable::version`].
    version: String,
}

impl Default for FxTable {
    fn default() -> Self {
        Self {
            base: "USD".to_string(),
            rates: HashMap::new(),
            version: "usd-only".to_string(),
        }
    }
}

/// On-disk shape of `config/fx_rates.json`.
#[derive(Debug, Deserialize)]
struct FxFile {
    #[serde(default = "default_base")]
    base: String,
    #[serde(default)]
    rates: HashMap<String, f64>,
    /// Provenance block. Only `last_verified` is read: it is the operator's own statement of when
    /// the rates were checked, which is a better book version than anything we could invent.
    #[serde(default, rename = "_meta")]
    meta: FxMeta,
}

#[derive(Debug, Default, Deserialize)]
struct FxMeta {
    #[serde(default)]
    last_verified: Option<String>,
}

fn default_base() -> String {
    "USD".to_string()
}

/// Name a book: the operator's own `_meta.last_verified` when the file carries one, else a content
/// hash of the base and the sorted rates.
///
/// `last_verified` is preferred because it is the statement a human made about these numbers, and
/// that is what someone auditing a repriced row wants to read. The hash is the fallback rather than
/// the primary so two files verified on different days with identical rates stay distinguishable —
/// re-verification is itself information.
fn book_version(last_verified: Option<&str>, base: &str, rates: &HashMap<String, f64>) -> String {
    if let Some(v) = last_verified.map(str::trim).filter(|s| !s.is_empty()) {
        return v.to_string();
    }
    if rates.is_empty() {
        return format!("{}-only", base.to_lowercase());
    }
    let mut pairs: Vec<(&String, &f64)> = rates.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = Sha256::new();
    h.update(base.as_bytes());
    for (code, rate) in pairs {
        h.update([0u8]);
        h.update(code.as_bytes());
        h.update(b"=");
        // Fixed-width decimal, not `{rate}`: a float's shortest round-trip rendering is stable
        // within a Rust version but is not a format anyone should pin a stored version string to.
        h.update(format!("{rate:.10}").as_bytes());
    }
    let digest = h.finalize();
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

impl FxTable {
    /// Parse the on-disk `fx_rates.json`. Codes are upper-cased; a stray `base` entry inside `rates`
    /// and any non-positive rate are dropped (a zero/negative rate would corrupt every conversion).
    pub fn from_json_str(s: &str) -> Result<Self, serde_json::Error> {
        let parsed: FxFile = serde_json::from_str(s)?;
        let base = parsed.base.to_uppercase();
        let rates: HashMap<String, f64> = parsed
            .rates
            .into_iter()
            .map(|(k, v)| (k.to_uppercase(), v))
            .filter(|(k, v)| *k != base && *v > 0.0)
            .collect();
        let version = book_version(parsed.meta.last_verified.as_deref(), &base, &rates);
        Ok(Self {
            base,
            rates,
            version,
        })
    }

    /// Load from `LIGHTTRACK_FX_RATES` (or `config/fx_rates.json`). A missing or unparseable file
    /// falls back to a USD-only table — non-USD revenue is then stored 1:1 and flagged, never dropped.
    pub fn from_env() -> Self {
        let path = std::env::var("LIGHTTRACK_FX_RATES")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "config/fx_rates.json".to_string());
        match std::fs::read_to_string(&path) {
            Ok(s) => Self::from_json_str(&s).unwrap_or_else(|e| {
                eprintln!(
                    "fx rates parse error in '{path}': {e}; USD-only \
                     (non-USD revenue stored 1:1 and flagged)"
                );
                Self::default()
            }),
            Err(_) => {
                eprintln!(
                    "fx rates file '{path}' not found; USD-only \
                     (non-USD revenue stored 1:1 and flagged)"
                );
                Self::default()
            }
        }
    }

    /// Build directly from a base + rate map (tests, and any programmatic seeding).
    pub fn new(base: impl Into<String>, rates: HashMap<String, f64>) -> Self {
        let base = base.into().to_uppercase();
        let rates: HashMap<String, f64> = rates
            .into_iter()
            .map(|(k, v)| (k.to_uppercase(), v))
            .filter(|(k, v)| *k != base && *v > 0.0)
            .collect();
        let version = book_version(None, &base, &rates);
        Self {
            base,
            rates,
            version,
        }
    }

    /// A stable name for this book, stamped on every row it converts.
    ///
    /// Rows had no way to say *which* rates produced them, so "we fixed the EUR rate" and "these
    /// rows already carry the fixed rate" were the same sentence, and the only remedy on offer was
    /// re-ingesting from the provider. With a version on the row, `reprice_revenue` can find exactly
    /// the rows a book change should touch and leave the rest alone.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Normalize `minor_units` of `currency` into USD, honoring the currency's decimal places and its
    /// FX rate. The base currency and any currency with a rate convert genuinely; an unknown currency
    /// is returned at 1:1 with `converted = false`.
    pub fn to_usd(&self, minor_units: i64, currency: &str) -> UsdAmount {
        let cur = currency.to_uppercase();
        let major = minor_units as f64 / minor_divisor(&cur);
        if cur == self.base {
            return UsdAmount {
                amount_usd: major,
                converted: true,
                rate: Some(1.0),
            };
        }
        match self.rates.get(&cur) {
            Some(rate) => UsdAmount {
                amount_usd: major * rate,
                converted: true,
                rate: Some(*rate),
            },
            None => UsdAmount {
                amount_usd: major,
                converted: false,
                rate: None,
            },
        }
    }

    /// The USD value of one major unit of `currency`: `1.0` for the base, the book's entry for a
    /// convertible currency, `None` when the book has no rate. What a reprice reads to find out
    /// whether the rate an operator has just added is actually loaded.
    pub fn rate_for(&self, currency: &str) -> Option<f64> {
        let cur = currency.to_uppercase();
        if cur == self.base {
            return Some(1.0);
        }
        self.rates.get(&cur).copied()
    }

    /// Whether `currency` converts to USD without a 1:1 fallback (it is the base, or has a rate).
    pub fn is_convertible(&self, currency: &str) -> bool {
        let cur = currency.to_uppercase();
        cur == self.base || self.rates.contains_key(&cur)
    }
}

/// Process-wide FX table, loaded once from the environment. Shared by the billing adapters (at ingest)
/// and the margin surface (to flag unconverted currencies) so both agree on the same rate book.
pub fn shared_fx() -> Arc<FxTable> {
    static SHARED: OnceLock<Arc<FxTable>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(FxTable::from_env())).clone()
}

/// The number of minor units in one major unit of `currency` (i.e. `10^decimals`). ISO-4217 common
/// set: 0-decimal and 3-decimal currencies; everything else is the 2-decimal default.
fn minor_divisor(currency: &str) -> f64 {
    let exp = match currency {
        // Zero-decimal: the minor unit *is* the major unit (no cents).
        "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF" | "UGX"
        | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0i32,
        // Three-decimal (Gulf/Maghreb dinars).
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
        _ => 2,
    };
    10f64.powi(exp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> FxTable {
        let mut r = HashMap::new();
        r.insert("EUR".to_string(), 1.10); // 1 EUR = $1.10
        r.insert("JPY".to_string(), 0.0064); // 1 JPY = $0.0064
        FxTable::new("USD", r)
    }

    #[test]
    fn usd_is_native_and_two_decimal() {
        let a = table().to_usd(2000, "usd");
        assert!((a.amount_usd - 20.0).abs() < 1e-9);
        assert!(a.converted);
    }

    #[test]
    fn jpy_is_zero_decimal_not_divided_by_100() {
        // 2000 minor JPY = ¥2000 (no cents), converted at 0.0064 → $12.80.
        let a = table().to_usd(2000, "JPY");
        assert!((a.amount_usd - 12.8).abs() < 1e-9, "got {}", a.amount_usd);
        assert!(a.converted);
    }

    #[test]
    fn eur_converts_at_table_rate() {
        // 2000 minor EUR = €20.00, at 1.10 → $22.00.
        let a = table().to_usd(2000, "eur");
        assert!((a.amount_usd - 22.0).abs() < 1e-9, "got {}", a.amount_usd);
        assert!(a.converted);
    }

    #[test]
    fn three_decimal_currency_divides_by_1000() {
        // 20000 minor KWD = 20.000 KWD; no rate → 1:1 fallback, flagged.
        let a = table().to_usd(20000, "KWD");
        assert!((a.amount_usd - 20.0).abs() < 1e-9, "got {}", a.amount_usd);
        assert!(!a.converted, "no KWD rate → unconverted");
    }

    #[test]
    fn unknown_currency_falls_back_1to1_and_is_flagged() {
        let a = table().to_usd(2000, "GBP");
        assert!((a.amount_usd - 20.0).abs() < 1e-9);
        assert!(!a.converted, "GBP has no rate → flagged as unconverted");
        assert!(!table().is_convertible("GBP"));
        assert!(table().is_convertible("EUR"));
        assert!(table().is_convertible("USD"));
    }

    #[test]
    fn parses_json_and_drops_bad_rates() {
        let t = FxTable::from_json_str(
            r#"{ "base": "usd", "rates": { "eur": 1.08, "USD": 2.0, "BAD": 0.0, "GBP": 1.27 } }"#,
        )
        .unwrap();
        assert!(t.is_convertible("EUR"));
        assert!(t.is_convertible("GBP"));
        assert!(!t.is_convertible("BAD"), "zero rate dropped");
        // A stray USD inside rates must not shadow the base's implicit 1.0.
        let a = t.to_usd(100, "USD");
        assert!(
            (a.amount_usd - 1.0).abs() < 1e-9,
            "USD stays 1:1, got {}",
            a.amount_usd
        );
    }
}
