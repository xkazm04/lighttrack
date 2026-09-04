//! What this device can actually run: the action library, enumerated (M18).
//!
//! The cloud used to hand a task to whoever asked first. That is fine with one device and wrong
//! with two — a device whose library has no `xprice/reprice-summary` would lease it anyway, spend a
//! real attempt discovering the folder is not there, and wait out a five-hour retry interval before
//! the next device got a chance. So the agent tells the cloud what it holds, and the lease is
//! filtered against it.
//!
//! The inventory is **derived from the filesystem, not configured**. A hand-maintained capability
//! list in `agent.toml` goes stale the moment somebody adds an action folder, and a stale list is
//! precisely the routing failure this exists to end — it would send work to a device that cannot run
//! it, or withhold work from one that can.

use std::path::Path;

/// Every `<ns>/<name>` under `actions_dir` that has a `prompt.md`.
///
/// `prompt.md` is the requirement because it is what `actions::load` requires: an action without one
/// cannot run, so advertising it would be advertising a failure. Two levels deep exactly, matching
/// the library layout (`actions/<ns>/<action>/`) and the `action_type` contract.
///
/// An unreadable directory yields an empty inventory rather than an error, and the caller treats
/// that as "advertise nothing" — which the cloud reads as **no filter**. That is the deliberate
/// back-compat direction: a device that could not enumerate itself keeps leasing everything, exactly
/// as it did before M18, instead of silently going idle over a permissions problem.
///
/// That direction is right and stays. What it must not also do is hide WHY the inventory is empty.
/// "I read the library and it holds nothing runnable" and "I could not read the library at all" are
/// different states with opposite remedies — add an action, versus fix a path or a permission — and
/// this function collapses them into the same empty vector on purpose, because the cloud's routing
/// must not change. `unreadable_reason` recovers the distinction for the operator WITHOUT changing
/// what is advertised: the lease filter still sees "nothing", and the banner no longer has to
/// pretend that is the whole story. Keeping the fix on the reporting side rather than the routing
/// side is the point — permission to lease everything and the claim to have enumerated successfully
/// are separate facts, and only the second one was ever in doubt.
pub(crate) fn inventory(actions_dir: &str) -> Vec<String> {
    let root = Path::new(actions_dir);
    let mut found = Vec::new();
    let Ok(namespaces) = std::fs::read_dir(root) else {
        return found;
    };
    for ns in namespaces.flatten() {
        if !ns.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let ns_name = ns.file_name().to_string_lossy().to_string();
        // `_example` and friends are scaffolding, not actions somebody meant to expose.
        if ns_name.starts_with('_') || ns_name.starts_with('.') {
            continue;
        }
        let Ok(actions) = std::fs::read_dir(ns.path()) else {
            continue;
        };
        for a in actions.flatten() {
            if !a.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = a.file_name().to_string_lossy().to_string();
            if name.starts_with('_') || name.starts_with('.') {
                continue;
            }
            if a.path().join("prompt.md").is_file() {
                found.push(format!("{ns_name}/{name}"));
            }
        }
    }
    found.sort();
    found
}

/// Why the library could not be enumerated, or `None` when it was read successfully.
///
/// Deliberately separate from `inventory` and deliberately advisory: it never changes what the
/// device advertises, it only lets the startup banner say which of the two empty states this is.
/// The banner exists to answer "why is nothing being picked up" without reading the cloud's logs
/// (see `main`), and a mistyped `actions_dir` is the likeliest cause of that question — the one
/// cause the banner could not name.
///
/// The asymmetry this closes was not designed. One line above the inventory call, `main` probes the
/// engine binary and refuses to start with a message naming the path AND the config file it came
/// from. Two capability checks, one line apart: the missing binary is fatal and actionable, the
/// missing library was silent. The remedy is not to make the second fatal — it must not be — but to
/// make it as legible.
pub(crate) fn unreadable_reason(actions_dir: &str) -> Option<String> {
    match std::fs::read_dir(Path::new(actions_dir)) {
        Ok(_) => None,
        Err(e) => Some(e.to_string()),
    }
}

/// One line for the startup banner, so an operator can see what this device is offering before it
/// has leased anything — the question "why is nothing being picked up" should be answerable without
/// reading the cloud's logs.
pub(crate) fn describe(actions: &[String]) -> String {
    match actions.len() {
        0 => {
            "none (this device advertises nothing, so the cloud will not filter its leases)".into()
        }
        n if n <= 6 => actions.join(", "),
        n => format!("{} … ({n} actions)", actions[..6].join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(root: &Path, path: &str, with_prompt: bool) {
        let dir = root.join(path);
        std::fs::create_dir_all(&dir).expect("mkdir");
        if with_prompt {
            std::fs::write(dir.join("prompt.md"), "hi").expect("write prompt");
        }
    }

    #[test]
    fn the_inventory_is_the_runnable_actions_and_only_those() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        action(root, "xprice/reprice-summary", true);
        action(root, "xprice/deep-scan", true);
        action(root, "ops/nightly", true);
        // A folder with no prompt.md cannot run, so advertising it would advertise a failure —
        // and the cloud would route work to a device that is going to report "no action".
        action(root, "xprice/half-built", false);
        // Scaffolding is not an action somebody meant to expose.
        action(root, "_example/thing", true);
        action(root, "ops/_wip", true);

        let got = inventory(root.to_str().expect("utf8 path"));
        assert_eq!(
            got,
            vec![
                "ops/nightly".to_string(),
                "xprice/deep-scan".into(),
                "xprice/reprice-summary".into()
            ],
            "sorted, two levels deep, prompt.md required"
        );
    }

    #[test]
    fn a_library_that_cannot_be_read_advertises_nothing_rather_than_failing() {
        // "Advertise nothing" is read by the cloud as "no filter", so a device with a missing or
        // unreadable library keeps leasing everything exactly as it did before M18 — a wrong
        // permission must not silently take a device out of the fleet.
        assert!(inventory("no/such/library").is_empty());
        assert!(describe(&[]).contains("will not filter"));
    }

    #[test]
    fn an_unreadable_library_is_distinguishable_from_an_empty_one() {
        // The two causes of an empty inventory have opposite remedies, and the banner is where an
        // operator looks first. Routing is unchanged — both still advertise nothing — but only one
        // of them is a mistyped path, and that one now says so.
        let tmp = tempfile::tempdir().expect("tempdir");
        let empty_but_readable = tmp.path().to_str().expect("utf8 path");

        assert!(inventory(empty_but_readable).is_empty());
        assert!(inventory("no/such/library").is_empty());

        assert_eq!(
            unreadable_reason(empty_but_readable),
            None,
            "a readable directory holding no actions is not an error"
        );
        assert!(
            unreadable_reason("no/such/library").is_some(),
            "a library that cannot be enumerated names why"
        );
    }

    #[test]
    fn the_banner_summarises_a_large_library_instead_of_printing_all_of_it() {
        let few: Vec<String> = (0..3).map(|i| format!("ns/a{i}")).collect();
        assert_eq!(describe(&few), "ns/a0, ns/a1, ns/a2");
        let many: Vec<String> = (0..20).map(|i| format!("ns/a{i:02}")).collect();
        let line = describe(&many);
        assert!(line.contains("(20 actions)"), "{line}");
        assert!(line.starts_with("ns/a00, "), "{line}");
    }
}
