//! Policy over the deployment shape: what `deploy/helm/lighttrack/` may never grant.
//!
//! THE GAP THIS CLOSES. `deploy/README.md` lists seven deployment surfaces and marks all of them
//! available. The Helm chart is the one a stranger runs against their own cluster, and until this
//! file existed it was the only shipped artifact **nothing read**: `.ai/manifest.yaml`'s
//! `controls.ciHardPass` listed nine gates and not one of them opened `deploy/`. The chart said so
//! itself in its first three lines — *"UNVERIFIED template (authored without a local helm to
//! lint)"*.
//!
//! `helm lint` would not have closed it either. It has no opinion about replica counts, and `helm
//! template` renders a privileged pod as happily as an unprivileged one. Two of the rules below are
//! data-integrity rules rather than hardening preferences, which is why this is a gate and not a
//! linter's default set:
//!
//! * **ONE WRITER.** State is a single SQLite file on a ReadWriteOnce volume unless
//!   `secrets.databaseUrl` is set. Two pods are two writers on one file — a corrupt database, not a
//!   scaling limit — and a `strategy:` block that is absent or `RollingUpdate` is the same failure
//!   for the length of a deploy. The invariant is CONDITIONAL, which is exactly why it has to be
//!   written down: a rule that only sometimes applies is a rule nobody applies from memory.
//! * **THE PROBE SPLIT.** `crates/api/src/health.rs` answers liveness and readiness separately
//!   because a dependency check behind a restarter is a crash loop and a constant behind a router
//!   is a false green. That is one edit away from being undone in this template, by someone tidying
//!   two probes into one path.
//!
//! WHY IT IS A RUST TEST. Every gate in this repository that reads a text file it does not own is
//! one — `gate_table_guard.rs` reads the CI workflow, `manifest_guard.rs` reads `.ai/manifest.yaml`
//! — for the same reason: `cargo test --workspace` already blocks, `lighttrack-core` stays
//! dependency-light (deny.toml's bans policy), and there is no repo-root `package.json` to hang a
//! node script off. The design is copied from `kp/scripts/deploy/check-chart.mjs`, whose two
//! decisions are the ones that make it runnable at all:
//!
//! 1. **Read chart TEXT, not rendered YAML.** Half of these files are `{{- if }}` / `{{- with }}`
//!    and do not parse until Helm has rendered them. Two readers — the literal value of a key, and
//!    the keys a block declares — carry every policy below.
//! 2. **Anchor every rule to BOTH the values and the template.** A `securityContext` block nothing
//!    mounts is decoration, and checking only the values would call it hardened. The mirror of that
//!    trap is what this chart actually shipped: `resources: {}` emitted under `{{- with }}`, so a
//!    values file that looked like it said nothing produced a BestEffort pod.
//!
//! THE HOLE THIS APPROACH HAS, stated rather than discovered later. These are text matches over Go
//! template source, so moving a checked value into `_helpers.tpl` and including it would satisfy
//! the pod at runtime while the reader here sees nothing. That failure is **fail-closed**: the
//! policy goes red and a human looks. The dangerous direction — the checker still seeing a string
//! the pod no longer gets — is only reachable by leaving the reference in place while gutting the
//! helper, and `helm template` in CI is the answer if that ever happens. It is not the answer today,
//! because adding a helm binary to CI is a larger decision and the useful half needs neither.
//!
//! CHANGING A POLICY is a deliberate edit to `POLICIES` below carrying its reason, which a reviewer
//! can read and disagree with. Loosening a value in `values.yaml` until this goes quiet is the
//! failure mode this file exists to prevent.

const VALUES: &str = include_str!("../../../deploy/helm/lighttrack/values.yaml");
const DEPLOYMENT: &str = include_str!("../../../deploy/helm/lighttrack/templates/deployment.yaml");
const SERVICE: &str = include_str!("../../../deploy/helm/lighttrack/templates/service.yaml");
const SECRET: &str = include_str!("../../../deploy/helm/lighttrack/templates/secret.yaml");
const CHART: &str = include_str!("../../../deploy/helm/lighttrack/Chart.yaml");

/// The chart as text. `include_str!` means a renamed or deleted file is a **build** error rather
/// than a quiet pass — the equivalent of kp's exit code 2.
#[derive(Clone)]
struct Chart {
    values: String,
    deployment: String,
    service: String,
    secret: String,
    chart: String,
}

impl Chart {
    fn shipped() -> Self {
        Self {
            values: VALUES.to_string(),
            deployment: DEPLOYMENT.to_string(),
            service: SERVICE.to_string(),
            secret: SECRET.to_string(),
            chart: CHART.to_string(),
        }
    }
}

// --- reading YAML that is also a Go template ----------------------------------------------------

/// Strip a trailing `# comment` from a scalar value.
fn scalar(v: &str) -> &str {
    match v.find(" #") {
        Some(i) => v[..i].trim(),
        None => v.trim(),
    }
}

/// The indented body of a top-level `key:` block, or `""` when there is none.
///
/// Ends at the first non-blank line back at column 0, which is what makes `block_of(values,
/// "resources")` stop at the next top-level key instead of swallowing the rest of the file.
fn block_of(yaml: &str, key: &str) -> String {
    let head = format!("{key}:");
    let mut lines = yaml.lines();
    for line in lines.by_ref() {
        let l = line.trim_end();
        if l == head || (l.starts_with(&head) && scalar(&l[head.len()..]).is_empty()) {
            break;
        }
        if l.starts_with(&head) {
            // `key: value` on one line — an inline scalar, so the block is that value.
            return l[head.len()..].trim().to_string();
        }
    }
    let mut body = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            break;
        }
        body.push(line);
    }
    body.join("\n")
}

/// The literal value of the first `key:` at any depth of `text`, or `None`.
///
/// "At any depth" is deliberate: it is what makes `capabilities.drop` reachable as
/// `value_of(&block_of(values, "securityContext"), "drop")`. Each policy applies it to a block it
/// has already narrowed.
fn value_of(text: &str, key: &str) -> Option<String> {
    let head = format!("{key}:");
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(&head) {
            let v = scalar(rest);
            return Some(v.trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

/// True when the template refuses an illegal combination involving `value` with `{{ fail }}`.
///
/// The shape matched is a `{{- if ... }}` naming both `value` and `.Values.secrets.databaseUrl`
/// (the condition that lifts the single-writer invariant) immediately followed by a `{{- fail }}`.
/// Anchoring on the pair rather than on "the file contains the word fail somewhere" is the
/// difference between checking a guard and checking that some other guard exists.
fn guarded_by_fail(template: &str, value: &str) -> bool {
    let lines: Vec<&str> = template.lines().collect();
    lines.windows(2).any(|w| {
        let cond = w[0];
        cond.trim_start().starts_with("{{- if")
            && cond.contains(value)
            && cond.contains(".Values.secrets.databaseUrl")
            && w[1].trim_start().starts_with("{{- fail")
    })
}

/// True when `key:` appears anywhere in `text` — "is it wired at all".
fn has_key(text: &str, key: &str) -> bool {
    text.lines()
        .any(|l| l.trim_start().starts_with(&format!("{key}:")))
}

/// Literals that are a live credential rather than a placeholder.
const CREDENTIAL_SHAPES: &[(&str, &str)] = &[
    ("sk-", "an OpenAI-style key"),
    ("ghp_", "a GitHub token"),
    ("AIza", "a Google API key"),
    ("xoxb-", "a Slack token"),
    (
        "postgres://postgres:",
        "a Postgres DSN with an inline password",
    ),
];

// --- the policies -------------------------------------------------------------------------------

struct Policy {
    rule: &'static str,
    /// Why this property must not regress. Not decoration: a test below asserts every policy has
    /// one, because a rule whose reason nobody wrote down is a rule the next person deletes.
    why: &'static str,
    check: fn(&Chart) -> Option<String>,
}

const POLICIES: &[Policy] = &[
    Policy {
        rule: "replicas-conditional",
        why: "SQLite is one writer on a ReadWriteOnce volume; a second pod corrupts the database. \
              The invariant lifts only when secrets.databaseUrl moves state to Postgres, and CI \
              never sees the operator's own `-f my-values.yaml` — so the enforcement point is a \
              `{{ fail }}` in the template and this policy checks the guard is still there.",
        check: |c| {
            if !guarded_by_fail(&c.deployment, ".Values.replicaCount") {
                return Some(
                    "templates/deployment.yaml no longer refuses `replicaCount > 1` without \
                     secrets.databaseUrl. A templated replica count with nothing guarding it \
                     reintroduces two writers on one SQLite file."
                        .into(),
                );
            }
            match value_of(&c.values, "replicaCount").as_deref() {
                Some("1") => None,
                other => Some(format!(
                    "values.yaml defaults replicaCount to {}, not 1. The shipped default must be \
                     the safe one.",
                    other.unwrap_or("nothing")
                )),
            }
        },
    },
    Policy {
        rule: "no-update-strategy",
        why: "A rolling update runs the old and new pod together, which is the same two-writer \
              failure for the length of a deploy. Even at one replica.",
        check: |c| {
            if !c.deployment.contains("strategy:") || !c.deployment.contains(".Values.updateStrategy")
            {
                return Some(
                    "the Deployment declares no `strategy:` driven by .Values.updateStrategy. \
                     Kubernetes' default is RollingUpdate, which overlaps two pods on one RWO \
                     volume."
                        .into(),
                );
            }
            match value_of(&c.values, "updateStrategy").as_deref() {
                Some("Recreate") => None,
                other => Some(format!(
                    "values.yaml defaults updateStrategy to {}, not Recreate.",
                    other.unwrap_or("nothing")
                )),
            }
        },
    },
    Policy {
        rule: "resources-not-applied",
        why: "An empty `resources` map is not the defaults — it is BestEffort QoS: first evicted \
              under node pressure and unbounded on the way there, running judge workloads. The \
              template used to emit it under `{{- with }}`, so an empty map produced no resources \
              block at all.",
        check: |c| {
            if !c.deployment.contains(".Values.resources") {
                return Some("the Deployment does not apply .Values.resources.".into());
            }
            if c.deployment.contains("{{- with .Values.resources }}") {
                return Some(
                    "the Deployment emits resources under `{{- with }}` again: an empty map then \
                     produces no resources block at all, silently."
                        .into(),
                );
            }
            let res = block_of(&c.values, "resources");
            if !has_key(&res, "requests") || !has_key(&res, "limits") {
                return Some(format!(
                    "values.yaml `resources` declares {}. Both a request (so the scheduler can \
                     place the pod) and a limit (so it cannot take the node with it) are required.",
                    if res.trim().is_empty() {
                        "nothing".to_string()
                    } else {
                        format!("no {}", if has_key(&res, "requests") { "limits" } else { "requests" })
                    }
                ));
            }
            if !res.contains("memory") {
                return Some("values.yaml `resources` declares no memory figure.".into());
            }
            None
        },
    },
    Policy {
        rule: "no-pod-security-context",
        why: "The image already builds and runs as uid 10001 (deploy/docker/Dockerfile). Without a \
              podSecurityContext the cluster only trusts that; with one it enforces it, and a \
              future base-image change cannot quietly hand the pod uid 0.",
        check: |c| {
            for wired in [".Values.podSecurityContext", ".Values.securityContext"] {
                if !c.deployment.contains(wired) {
                    return Some(format!(
                        "the Deployment no longer applies {wired}. The values can say anything \
                         once nothing reads them."
                    ));
                }
            }
            let pod = block_of(&c.values, "podSecurityContext");
            if value_of(&pod, "runAsNonRoot").as_deref() != Some("true") {
                return Some("podSecurityContext.runAsNonRoot is not `true`.".into());
            }
            match value_of(&pod, "runAsUser").as_deref() {
                None => return Some("podSecurityContext.runAsUser is unset.".into()),
                Some("0") => return Some("podSecurityContext.runAsUser is root (0).".into()),
                Some(_) => {}
            }
            let ctr = block_of(&c.values, "securityContext");
            if value_of(&ctr, "allowPrivilegeEscalation").as_deref() != Some("false") {
                return Some("securityContext.allowPrivilegeEscalation is not `false`.".into());
            }
            match value_of(&ctr, "drop") {
                Some(d) if d.contains("ALL") => None,
                other => Some(format!(
                    "securityContext.capabilities.drop does not drop ALL (found {}).",
                    other.unwrap_or_else(|| "nothing".into())
                )),
            }
        },
    },
    Policy {
        rule: "service-account-token-mounted",
        why: "tracklight calls no Kubernetes API. Mounting the namespace default ServiceAccount's \
              token hands a cluster credential to a pod that has no use for one — the classic \
              lateral-movement step from a compromised web process.",
        check: |c| {
            if !c.deployment.contains("automountServiceAccountToken") {
                return Some(
                    "the pod spec sets no `automountServiceAccountToken`. Absent means true: the \
                     default ServiceAccount's token is mounted."
                        .into(),
                );
            }
            match value_of(&c.values, "automountServiceAccountToken").as_deref() {
                Some("false") => None,
                other => Some(format!(
                    "automountServiceAccountToken defaults to {}, not false.",
                    other.unwrap_or("nothing")
                )),
            }
        },
    },
    Policy {
        rule: "probes-not-collapsed",
        why: "Liveness and readiness answer different questions with opposite remedies (see \
              crates/api/src/health.rs). Both probes reading one path is the defect this chart \
              shipped with: a constant behind the router is a false green, and a dependency check \
              behind the restarter is a crash loop. One tidy-up undoes it.",
        check: |c| {
            let dep = &c.deployment;
            for (probe, path) in [
                ("livenessProbe", "/health/live"),
                ("readinessProbe", "/health/ready"),
                ("startupProbe", "/health/live"),
            ] {
                let Some(at) = dep.find(&format!("{probe}:")) else {
                    return Some(format!("the Deployment declares no {probe}."));
                };
                let window = &dep[at..dep.len().min(at + 400)];
                if !window.contains(path) {
                    return Some(format!(
                        "{probe} does not read {path}. Liveness must observe nothing outside the \
                         process; readiness must observe the store."
                    ));
                }
            }
            if !c.values.contains("probes:") {
                return Some(
                    "values.yaml declares no `probes:` block — the thresholds an operator with a \
                     slow database has to raise are back inside the template."
                        .into(),
                );
            }
            None
        },
    },
    Policy {
        rule: "secret-literal-in-values",
        why: "values.yaml is the file people paste into tickets, commit to their infra repo and \
              share. Green today; the rule is what keeps it that way.",
        check: |c| {
            let secrets = block_of(&c.values, "secrets");
            for key in ["adminKey", "databaseUrl"] {
                if !value_of(&secrets, key).unwrap_or_default().is_empty() {
                    return Some(format!("secrets.{key} ships with a value."));
                }
            }
            for (shape, what) in CREDENTIAL_SHAPES {
                if c.values.contains(shape) {
                    return Some(format!("values.yaml contains {what}."));
                }
            }
            // The Secret template must take its material from values, never carry a literal.
            for (shape, what) in CREDENTIAL_SHAPES {
                if c.secret.contains(shape) {
                    return Some(format!("templates/secret.yaml contains {what}."));
                }
            }
            None
        },
    },
    Policy {
        rule: "service-exposed-by-default",
        why: "A default install should not put itself on a node port or ask a cloud for a public \
              load balancer. Expose it through the chart's ingress (and its TLS) instead.",
        check: |c| {
            if !c.service.contains(".Values.service.type") {
                return Some(
                    "templates/service.yaml no longer applies .Values.service.type — the value                      below is then decoration."
                        .into(),
                );
            }
            match value_of(&block_of(&c.values, "service"), "type").as_deref() {
                Some("ClusterIP") => None,
                other => Some(format!(
                    "service.type defaults to {}, not ClusterIP.",
                    other.unwrap_or("nothing")
                )),
            }
        },
    },
    Policy {
        rule: "volume-access-mode",
        why: "ReadWriteMany would let the cluster schedule the second writer the two rules above \
              exist to prevent.",
        check: |c| {
            if c.deployment.contains("ReadWriteOnce") {
                None
            } else {
                Some(
                    "the PersistentVolumeClaim no longer declares accessModes \
                     [\"ReadWriteOnce\"]."
                        .into(),
                )
            }
        },
    },
    Policy {
        rule: "image-version-coherence",
        why: "Three version strings — values.yaml image.tag, Chart.yaml appVersion, Chart.yaml \
              version — and until this rule nothing compared any two of them, so an install could \
              pull an image the chart did not describe. Deliberately NOT compared against the crate \
              version: the workspace is 0.0.1 while the published image is a release tag, so the \
              two are different clocks and a rule pretending otherwise would be red forever.",
        check: |c| {
            let tag = value_of(&block_of(&c.values, "image"), "tag").unwrap_or_default();
            let app = value_of(&c.chart, "appVersion").unwrap_or_default();
            if tag.trim_start_matches('v') == app.trim_start_matches('v') {
                None
            } else {
                Some(format!(
                    "values.yaml image.tag is `{tag}` but Chart.yaml appVersion is `{app}` — the \
                     chart describes an image the install would not pull."
                ))
            }
        },
    },
];

/// One finding: the rule that fired, what it saw, and why the rule exists.
#[derive(Debug)]
struct Finding {
    rule: &'static str,
    message: String,
    fix: &'static str,
}

fn run_policies(chart: &Chart) -> Vec<Finding> {
    POLICIES
        .iter()
        .filter_map(|p| {
            (p.check)(chart).map(|message| Finding {
                rule: p.rule,
                message,
                fix: p.why,
            })
        })
        .collect()
}

fn render(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| format!("BLOCK  [{}] {}\n       {}", f.rule, f.message, f.fix))
        .collect::<Vec<_>>()
        .join("\n")
}

fn fired(findings: &[Finding], rule: &str) -> bool {
    findings.iter().any(|f| f.rule == rule)
}

// --- the gate -----------------------------------------------------------------------------------

#[test]
fn the_shipped_chart_satisfies_every_policy() {
    let findings = run_policies(&Chart::shipped());
    assert!(
        findings.is_empty(),
        "deploy/helm/lighttrack violates {} of {} deployment policies:\n{}\n\nFix the chart, or \
         change the policy in this file with the reason — never by loosening the value until this \
         goes quiet.",
        findings.len(),
        POLICIES.len(),
        render(&findings)
    );
}

#[test]
fn every_policy_carries_the_reason_it_exists() {
    for p in POLICIES {
        assert!(
            p.why.len() > 40,
            "{} has no stated reason — a rule nobody wrote a reason for is one the next person \
             deletes",
            p.rule
        );
    }
}

// --- the must-fail fixtures -----------------------------------------------------------------------
//
// The half that matters. Each case below breaks exactly one thing in a chart that is otherwise the
// SHIPPED one, and proves the policy that names it fires. A policy that cannot fail is a comment
// with an exit code — and these are text matches over Go-template source, so a refactor that moves a
// value into `_helpers.tpl` is exactly the edit that would silently stop them matching. The
// regressions are written the way they would actually arrive: `resources: {}` under a `{{- with }}`
// looks like the tidy-up a reviewer would approve, and it is the one that ships a BestEffort pod.

/// The shipped chart with one substitution applied to one file.
fn broken(file: fn(&mut Chart) -> &mut String, from: &str, to: &str) -> Vec<Finding> {
    let mut c = Chart::shipped();
    {
        let target = file(&mut c);
        assert!(
            target.contains(from),
            "the fixture's anchor `{from}` is not in the shipped chart any more — this test was \
             checking a file that has moved on"
        );
        *target = target.replace(from, to);
    }
    run_policies(&c)
}

#[test]
fn the_shipped_chart_is_the_fixture_baseline() {
    // Without this, every case below could be passing for the wrong reason.
    assert!(run_policies(&Chart::shipped()).is_empty());
}

#[test]
fn the_one_that_looks_like_a_tidy_up_dropping_the_replica_guard() {
    // The guard is a wall of template noise at the top of a Deployment. Deleting it renders
    // identically for every install that was already legal, which is exactly why nothing but a
    // policy notices — and the second `fail` below it stays, so "the file mentions fail" is not
    // enough to catch this.
    let mut c = Chart::shipped();
    let start = c
        .deployment
        .find("{{- if and (gt (int .Values.replicaCount) 1)")
        .expect("the replica guard is in the shipped template");
    let end = c.deployment[start..]
        .find(
            "{{- end }}
",
        )
        .map(|i| {
            start
                + i
                + "{{- end }}
"
                .len()
        })
        .expect("the guard block is closed");
    c.deployment.replace_range(start..end, "");
    let f = run_policies(&c);
    assert!(fired(&f, "replicas-conditional"), "{f:?}");
}

#[test]
fn a_replica_default_above_one() {
    let f = broken(|c| &mut c.values, "replicaCount: 1", "replicaCount: 3");
    assert!(fired(&f, "replicas-conditional"), "{f:?}");
}

#[test]
fn a_rolling_update_overlaps_two_writers_on_one_volume() {
    let f = broken(
        |c| &mut c.values,
        "updateStrategy: Recreate",
        "updateStrategy: RollingUpdate",
    );
    assert!(fired(&f, "no-update-strategy"), "{f:?}");

    let g = broken(
        |c| &mut c.deployment,
        "  strategy:\n    type: {{ .Values.updateStrategy }}\n",
        "",
    );
    assert!(fired(&g, "no-update-strategy"), "{g:?}");
}

#[test]
fn an_empty_resources_map_and_the_with_block_that_hides_it() {
    let f = broken(
        |c| &mut c.values,
        "resources:\n  requests: { cpu: 100m, memory: 128Mi }\n  limits:   { cpu: \"1\", memory: 512Mi }",
        "resources: {}",
    );
    assert!(fired(&f, "resources-not-applied"), "{f:?}");

    // The trap that made an empty map invisible rather than merely wrong.
    let g = broken(
        |c| &mut c.deployment,
        "          resources:\n            {{- toYaml .Values.resources | nindent 12 }}",
        "          {{- with .Values.resources }}\n          resources:\n            {{- toYaml . | nindent 12 }}\n          {{- end }}",
    );
    assert!(fired(&g, "resources-not-applied"), "{g:?}");
}

#[test]
fn hardened_values_the_deployment_stopped_applying_are_still_a_finding() {
    // The failure a values-only check could never see: the block is still there, still correct, and
    // nothing mounts it.
    let f = broken(
        |c| &mut c.deployment,
        "        {{- toYaml .Values.podSecurityContext | nindent 8 }}",
        "        {}",
    );
    assert!(fired(&f, "no-pod-security-context"), "{f:?}");
}

#[test]
fn running_as_root_or_as_uid_zero_by_another_name() {
    let f = broken(
        |c| &mut c.values,
        "runAsNonRoot: true",
        "runAsNonRoot: false",
    );
    assert!(fired(&f, "no-pod-security-context"), "{f:?}");
    let g = broken(|c| &mut c.values, "runAsUser: 10001", "runAsUser: 0");
    assert!(fired(&g, "no-pod-security-context"), "{g:?}");
    let h = broken(
        |c| &mut c.values,
        "allowPrivilegeEscalation: false",
        "allowPrivilegeEscalation: true",
    );
    assert!(fired(&h, "no-pod-security-context"), "{h:?}");
    let i = broken(|c| &mut c.values, "drop: [\"ALL\"]", "drop: [\"NET_RAW\"]");
    assert!(fired(&i, "no-pod-security-context"), "{i:?}");
}

#[test]
fn a_cluster_token_mounted_into_a_pod_that_calls_no_api() {
    let f = broken(
        |c| &mut c.deployment,
        "      automountServiceAccountToken: {{ .Values.automountServiceAccountToken }}\n",
        "",
    );
    assert!(fired(&f, "service-account-token-mounted"), "{f:?}");
    let g = broken(
        |c| &mut c.values,
        "automountServiceAccountToken: false",
        "automountServiceAccountToken: true",
    );
    assert!(fired(&g, "service-account-token-mounted"), "{g:?}");
}

#[test]
fn the_two_probes_tidied_back_onto_one_path() {
    // Verbatim the defect this chart shipped with, and the one a reviewer would wave through.
    let f = broken(
        |c| &mut c.deployment,
        "path: /health/ready",
        "path: /health",
    );
    assert!(fired(&f, "probes-not-collapsed"), "{f:?}");
    let g = broken(|c| &mut c.deployment, "path: /health/live", "path: /health");
    assert!(fired(&g, "probes-not-collapsed"), "{g:?}");
}

#[test]
fn a_secret_that_shipped_in_values_by_either_route() {
    let f = broken(|c| &mut c.values, "adminKey: \"\"", "adminKey: \"hunter2\"");
    assert!(fired(&f, "secret-literal-in-values"), "{f:?}");
    let g = broken(
        |c| &mut c.values,
        "podAnnotations: {}",
        "podAnnotations: { note: \"sk-abcdefghijklmnopqrstuvwx\" }",
    );
    assert!(
        fired(&g, "secret-literal-in-values"),
        "a credential-shaped literal anywhere in the file, not only in the keys we know to look at: {g:?}"
    );
}

#[test]
fn a_default_install_that_asks_a_cloud_for_a_load_balancer() {
    let f = broken(|c| &mut c.values, "type: ClusterIP", "type: LoadBalancer");
    assert!(fired(&f, "service-exposed-by-default"), "{f:?}");
}

#[test]
fn a_shared_volume_access_mode() {
    let f = broken(|c| &mut c.deployment, "ReadWriteOnce", "ReadWriteMany");
    assert!(fired(&f, "volume-access-mode"), "{f:?}");
}

#[test]
fn a_chart_that_describes_an_image_it_would_not_pull() {
    let f = broken(
        |c| &mut c.chart,
        "appVersion: \"0.0.4\"",
        "appVersion: \"0.0.9\"",
    );
    assert!(fired(&f, "image-version-coherence"), "{f:?}");
}

// --- the readers, pinned ---------------------------------------------------------------------

#[test]
fn a_block_stops_at_the_next_top_level_key_not_at_the_end_of_the_file() {
    let y = "service:\n  type: ClusterIP\n  port: 80\ningress:\n  enabled: false\n";
    let b = block_of(y, "service");
    assert!(b.contains("type: ClusterIP") && b.contains("port: 80"));
    assert!(
        !b.contains("enabled"),
        "a reader that ran on would attribute every later key to the wrong block: {b}"
    );
}

#[test]
fn a_value_is_read_without_its_trailing_comment_and_quotes_are_stripped() {
    assert_eq!(
        value_of(
            "service:\n  type: ClusterIP   # \"\" = cluster default\n",
            "type"
        )
        .as_deref(),
        Some("ClusterIP")
    );
    assert_eq!(
        value_of("secrets:\n  adminKey: \"\"   # REQUIRED\n", "adminKey").as_deref(),
        Some("")
    );
    assert_eq!(value_of("a:\n  b: 1\n", "missing"), None);
}
