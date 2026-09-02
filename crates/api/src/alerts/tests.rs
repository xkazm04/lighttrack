//! Cooldown-key behaviour, the two rate detectors, and the property the whole milestone is for:
//! two `Alerter`s over one store admit one alert.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use lighttrack_core::{AlertKind, LimitAction, LimitMetric, LimitStatus, LimitWindow, Severity};
use lighttrack_store::{AlertAdmission, SqliteStore, Store};

use super::{compose, routing, AlertConfig, Alerter};

fn alerter(cooldown_secs: u64) -> Alerter {
    cfg_alerter(cooldown_secs, 5, 300)
}

fn cfg_alerter(cooldown_secs: u64, error_threshold: u32, error_window_secs: u64) -> Alerter {
    Alerter {
        config: AlertConfig {
            webhook: Some("https://hook.test/x".into()),
            bench_webhook: None,
            ntfy: None,
            resend: None,
            webhook_key: None,
            cooldown: Duration::from_secs(cooldown_secs),
            error_threshold,
            error_window: Duration::from_secs(error_window_secs),
            score_window: 20,
            score_min_samples: 8,
            score_drop: 0.15,
            dev_destinations: false,
        },
        // The production constructor, not a bare client: see `super::http_client`.
        http: super::http_client(),
        last_sent: Mutex::new(HashMap::new()),
        error_windows: Mutex::new(HashMap::new()),
        score_windows: Mutex::new(HashMap::new()),
        store: OnceLock::new(),
    }
}

fn breach(project: &str) -> LimitStatus {
    LimitStatus {
        rule_id: "r1".into(),
        project_id: project.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        action: LimitAction::Alert,
        current: 2.0,
        threshold: 1.0,
        breached: true,
        ratio: 2.0,
        warn_at: None,
        warning: false,
        scope: None,
        basis: Default::default(),
        cost_evidence: None,
        shed_fraction: 0.0,
        shedding: false,
    }
}

fn warning(project: &str) -> LimitStatus {
    LimitStatus {
        warn_at: Some(0.8),
        warning: true,
        breached: false,
        current: 0.85,
        ratio: 0.85,
        ..breach(project)
    }
}

#[test]
fn warning_and_breach_have_independent_cooldowns() {
    let a = alerter(3600);
    let w = warning("p1");
    let b = breach("p1");
    assert!(
        a.should_send_key(&a.warn_key(&w)),
        "warning sends first time"
    );
    assert!(
        !a.should_send_key(&a.warn_key(&w)),
        "warning suppressed within cooldown"
    );
    assert!(
        a.should_send_key(&a.dedup_key(&b)),
        "breach still sends despite the earlier warning"
    );
}

#[test]
fn dedup_within_cooldown() {
    let a = alerter(3600);
    let b = breach("p1");
    assert!(a.should_send_key(&a.dedup_key(&b)));
    assert!(!a.should_send_key(&a.dedup_key(&b)));
    assert!(a.should_send_key(&a.dedup_key(&breach("p2"))));
}

#[test]
fn zero_cooldown_always_sends() {
    let a = alerter(0);
    let b = breach("p1");
    assert!(a.should_send_key(&a.dedup_key(&b)));
    assert!(a.should_send_key(&a.dedup_key(&b)));
}

#[test]
fn error_window_counts_and_evicts() {
    let a = cfg_alerter(3600, 3, 60);
    let base = Instant::now();
    assert_eq!(a.note_error("p", base), 1);
    assert_eq!(a.note_error("p", base + Duration::from_secs(30)), 2);
    // `base` is now 90s old (> 60s window) → evicted; `base+30` (60s old) kept; +new = 2.
    assert_eq!(a.note_error("p", base + Duration::from_secs(90)), 2);
    assert_eq!(a.note_error("q", base + Duration::from_secs(90)), 1);
}

#[test]
fn score_regression_detected() {
    let a = alerter(3600);
    for _ in 0..12 {
        assert!(a.note_score("p\u{1}helpfulness", 0.9).is_none());
    }
    let mut tripped = false;
    for _ in 0..4 {
        if a.note_score("p\u{1}helpfulness", 0.4).is_some() {
            tripped = true;
        }
    }
    assert!(tripped);
    // A steady-but-low rubric (no baseline-vs-recent gap) does not trip.
    for _ in 0..12 {
        assert!(a.note_score("p\u{1}steady", 0.5).is_none());
    }
}

/// **The milestone's headline property.** Two alerters — two API replicas — over one store, both
/// deciding the same breach fired. Their in-process cooldown maps are independent and both say
/// "send", exactly as in production; the store is what makes it one alert.
#[test]
fn two_alerters_over_one_store_admit_one_alert() {
    let store: Arc<dyn Store + Send + Sync> =
        Arc::new(SqliteStore::open_in_memory().expect("in-memory store"));
    store.init_schema().expect("schema");

    let replica_a = alerter(3600);
    let replica_b = alerter(3600);
    replica_a.attach_store(Arc::clone(&store));
    replica_b.attach_store(Arc::clone(&store));

    let b = breach("p1");
    assert!(replica_a.should_send_key(&replica_a.dedup_key(&b)));
    assert!(
        replica_b.should_send_key(&replica_b.dedup_key(&b)),
        "the second replica's own map knows nothing about the first's send"
    );

    let alert_a = compose::breach(&b, None, None, replica_a.dedup_key(&b));
    let alert_b = compose::breach(&b, None, None, replica_b.dedup_key(&b));
    assert_eq!(
        store
            .insert_alert_dedup(&alert_a, Duration::from_secs(3600))
            .expect("admit a"),
        AlertAdmission::Admitted
    );
    assert!(
        matches!(
            store
                .insert_alert_dedup(&alert_b, Duration::from_secs(3600))
                .expect("admit b"),
            AlertAdmission::Suppressed { .. }
        ),
        "the second replica must be suppressed — one condition, one alert"
    );
    assert_eq!(
        store
            .list_alerts(&lighttrack_store::AlertFilter {
                project: Some("p1".into()),
                ..Default::default()
            })
            .expect("list")
            .len(),
        1
    );
}

/// A rejection flush is a delta the counter has already discarded, so two consecutive flushes for
/// one project must both be admitted by the durable gate — a shared key would have the second one
/// `Suppressed` inside the cooldown and its count lost for good.
#[test]
fn consecutive_rejection_flushes_are_distinct_rows_not_a_suppressed_duplicate() {
    let store: Arc<dyn Store + Send + Sync> =
        Arc::new(SqliteStore::open_in_memory().expect("in-memory store"));
    store.init_schema().expect("schema");
    let buckets: HashMap<String, (u64, f64)> =
        HashMap::from([("p1:CostUsd:Hour:".to_string(), (3u64, 0.5f64))]);
    let t0 = chrono::Utc::now();
    let first = compose::ingest_rejected("p1", &buckets, t0);
    let second = compose::ingest_rejected("p1", &buckets, t0 + chrono::Duration::seconds(900));
    assert_ne!(first.dedup_key, second.dedup_key);
    for a in [&first, &second] {
        assert_eq!(
            store
                .insert_alert_dedup(a, Duration::from_secs(3600))
                .expect("admit"),
            AlertAdmission::Admitted,
            "every flush is a new fact, never a repeat of the last one"
        );
    }
}

/// The composed payload IS the delivered body, so a receiver written against the old hard-coded
/// `{event,text,content,...}` envelope keeps working — and the stored row is what was sent.
#[test]
fn the_stored_payload_is_the_body_a_receiver_gets() {
    let b = breach("p1");
    let a = compose::breach(&b, Some(&7), None, "p1:cost_usd:hour".into());
    assert_eq!(a.payload["event"], "limit_breach");
    assert_eq!(a.payload["rejected_count"], 7);
    assert_eq!(a.payload["text"], a.payload["content"]);
    assert!(a.payload["text"]
        .as_str()
        .unwrap_or_default()
        .contains("7 ingest attempt(s) rejected"));
    assert!(a.payload["breach"].is_object());
    assert_eq!(a.severity, Severity::Critical);
    assert_eq!(a.kind, AlertKind::LimitBreach);
    assert_eq!(compose::subject_of(&a), "LightTrack: limit breach in 'p1'");
}

/// A channel's severity floor is what makes per-project routing useful — an on-call SMS channel
/// should not receive an informational benchmark completion.
#[test]
fn a_severity_floor_narrows_which_channels_receive_an_alert() {
    let a = alerter(3600);
    let mut env = routing::env_channels(&a.config);
    assert_eq!(env.len(), 1);
    env[0].min_severity = Severity::Critical;

    let breach_alert = compose::breach(&breach("p1"), None, None, "k".into());
    let bench = compose::bench_run(&compose::BenchRunAlert {
        benchmark: "b".into(),
        run_id: "r".into(),
        status: "ok".into(),
        mean: None,
        baseline: None,
    });
    assert!(env[0].accepts(breach_alert.kind, breach_alert.severity));
    assert!(!env[0].accepts(bench.kind, bench.severity));
}

/// **Redirects are not followed.** `vet` refuses a private destination, but a *public* webhook that
/// answers `302 Location: http://169.254.169.254/...` would walk straight past it — the check ran on
/// the URL we were configured with, not on the one we would end up fetching. `Policy::none()` on the
/// client is what closes that, so it is asserted against a real socket rather than assumed from the
/// builder call.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    // Answers one request with a 302 pointing at the cloud metadata service. The request is drained
    // before replying: closing the socket under an in-flight body is a transport abort on Windows,
    // which would fail this test for a reason that has nothing to do with redirects.
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(
                    b"HTTP/1.1 302 Found\r\n\
                      Location: http://169.254.169.254/latest/meta-data\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                )
                .await;
            let _ = sock.flush().await;
            // Let the client read the response before the socket goes away.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    let alerter = alerter(3600);
    let resp = alerter
        .http
        .post(format!("http://{addr}/hook"))
        .body("{}")
        .send()
        .await
        .expect("the 302 itself must come back, not an error");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "the redirect must be handed to us unfollowed — a client that chased it would have \
         fetched the metadata service"
    );
}
