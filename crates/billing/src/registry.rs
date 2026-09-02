//! Configured billing webhook sources, keyed by provider, built from env. Held in the API state so
//! the webhook handler can look a provider up by its URL path segment.

use std::collections::HashMap;

use crate::polar::PolarSource;
use crate::stripe::StripeSource;
use crate::BillingSource;

#[derive(Default)]
pub struct BillingRegistry {
    sources: HashMap<&'static str, Box<dyn BillingSource>>,
}

impl BillingRegistry {
    /// Build from env. A provider is enabled when its webhook secret is set:
    /// `LIGHTTRACK_STRIPE_WEBHOOK_SECRET`, `LIGHTTRACK_POLAR_WEBHOOK_SECRET`.
    pub fn from_env() -> Self {
        let mut sources: HashMap<&'static str, Box<dyn BillingSource>> = HashMap::new();
        if let Some(secret) = non_empty_env("LIGHTTRACK_STRIPE_WEBHOOK_SECRET") {
            sources.insert("stripe", Box::new(StripeSource::new(secret)));
        }
        if let Some(secret) = non_empty_env("LIGHTTRACK_POLAR_WEBHOOK_SECRET") {
            // Apps key margin on their internal user id, echoed into Polar order `metadata.userId`;
            // override the key with `LIGHTTRACK_POLAR_CUSTOMER_META_KEY` if an app uses a different one.
            let source = match non_empty_env("LIGHTTRACK_POLAR_CUSTOMER_META_KEY") {
                Some(key) => PolarSource::with_customer_key(secret, key),
                None => PolarSource::new(secret),
            };
            sources.insert("polar", Box::new(source));
        }
        Self { sources }
    }

    pub fn get(&self, provider: &str) -> Option<&dyn BillingSource> {
        self.sources.get(provider).map(Box::as_ref)
    }

    /// Comma-free summary of configured providers, for the startup log.
    pub fn describe(&self) -> String {
        if self.sources.is_empty() {
            return "none".to_string();
        }
        let mut keys: Vec<&str> = self.sources.keys().copied().collect();
        keys.sort_unstable();
        keys.join("+")
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider is enabled by its secret and nothing else; an exported-but-empty secret is unset.
    /// Both env states run in ONE test so no parallel test can observe the mutation half-applied.
    #[test]
    fn a_provider_is_enabled_exactly_when_its_secret_is_set() {
        let keys = [
            "LIGHTTRACK_STRIPE_WEBHOOK_SECRET",
            "LIGHTTRACK_POLAR_WEBHOOK_SECRET",
        ];
        let saved: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();

        for k in keys {
            std::env::remove_var(k);
        }
        let none = BillingRegistry::from_env();
        assert_eq!(none.describe(), "none");
        assert!(none.get("stripe").is_none() && none.get("polar").is_none());

        std::env::set_var(keys[0], "whsec_x");
        std::env::set_var(keys[1], "");
        let stripe_only = BillingRegistry::from_env();
        assert_eq!(stripe_only.describe(), "stripe");
        assert!(
            stripe_only.get("polar").is_none(),
            "an empty secret enables nothing"
        );

        std::env::set_var(keys[1], "polar_x");
        let both = BillingRegistry::from_env();
        assert_eq!(both.describe(), "polar+stripe");
        assert_eq!(both.get("polar").map(|s| s.provider()), Some("polar"));

        for (k, v) in keys.iter().zip(saved) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
