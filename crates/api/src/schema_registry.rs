//! Name → JSON Schema, for the `TypeRef::Named` rows of the contract.
//!
//! `lighttrack-contract` names the DTOs it refers to as strings, because it has no dependency on
//! `core` — that is what keeps it linkable from the renderer. This module is the one place those
//! names are bound to real Rust types, and the test at the bottom asserts that *every* name the
//! table mentions resolves here. A `TypeRef::Named` nobody can resolve renders as a permissive
//! object in `/openapi.json`, which is exactly the vague document the item exists to replace, so
//! the omission is a failure rather than a degradation nobody notices.

use lighttrack_core as core;
use schemars::schema_for;
use serde_json::Value;

/// The JSON Schema for a DTO the contract names, or `None` when this build has no type for it.
pub(crate) fn schema_for_name(name: &str) -> Option<Value> {
    // One arm per name. A macro keeps the arm and the type adjacent, so adding a `TypeRef::Named`
    // row is a one-line change here and the test says so if it is forgotten.
    macro_rules! bind {
        ($($n:literal => $t:ty),+ $(,)?) => {
            match name {
                $($n => serde_json::to_value(schema_for!($t)).ok(),)+
                _ => None,
            }
        };
    }
    bind! {
        "Alert" => core::Alert,
        "AlertChannel" => core::AlertChannel,
        "ApiKey" => core::ApiKey,
        "Benchmark" => core::Benchmark,
        "BenchmarkRun" => core::BenchmarkRun,
        "CalibrationRecord" => core::CalibrationRecord,
        "CollectiveDigest" => core::CollectiveDigest,
        "ContributionRecord" => core::ContributionRecord,
        "CostByDimension" => core::CostByDimension,
        "Dataset" => core::Dataset,
        "DatasetItem" => core::DatasetItem,
        "Device" => core::Device,
        "Job" => core::Job,
        "Label" => core::Label,
        "LeaseHeld" => core::LeaseHeld,
        "LimitRule" => core::LimitRule,
        "LlmEvent" => core::LlmEvent,
        "MarginPolicy" => core::MarginPolicy,
        "ModelPriceRow" => core::ModelPriceRow,
        "Project" => core::Project,
        "Prompt" => core::Prompt,
        "PromptVersion" => core::PromptVersion,
        "RelayCancel" => core::RelayCancel,
        "RelayTask" => core::RelayTask,
        "RevenueEvent" => core::RevenueEvent,
        "Rubric" => core::Rubric,
        "Schedule" => core::Schedule,
        "Score" => core::Score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the module: the contract cannot name a type this build cannot describe.
    #[test]
    fn every_named_type_in_the_contract_resolves() {
        let missing: Vec<&str> = lighttrack_contract::openapi::named_types()
            .into_iter()
            .filter(|n| schema_for_name(n).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "the contract names these types but nothing here binds them, so /openapi.json would \
             describe them as bare objects: {missing:?}. Add an arm (and the `schemars::JsonSchema` \
             derive on the type) rather than leaving the document vague."
        );
    }

    /// …and the reverse: a binding for a name nobody uses is dead weight that will rot.
    #[test]
    fn no_binding_is_unreachable_from_the_contract() {
        let named = lighttrack_contract::openapi::named_types();
        for n in ["Alert", "ApiKey", "LlmEvent", "Score"] {
            assert!(
                schema_for_name(n).is_some(),
                "{n} must stay bound — the table's response rows point at it"
            );
        }
        assert!(!named.is_empty());
    }

    /// A derived schema must actually describe fields, not degenerate to `true`/`{}` — which is
    /// what a missing derive on a nested type used to produce silently.
    #[test]
    fn a_derived_schema_names_the_type_and_its_fields() {
        let s = schema_for_name("LlmEvent").expect("bound");
        assert_eq!(s["title"], "LlmEvent");
        let props = s["properties"].as_object().expect("object with fields");
        for f in ["id", "project_id", "ts", "provider", "model"] {
            assert!(props.contains_key(f), "LlmEvent schema is missing {f}");
        }
    }
}
