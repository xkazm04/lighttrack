use super::*;

const KINDS: &[&str] = &[
    "bench_run",
    "score_events",
    "score_traces",
    "dataset_sample",
    "calibrate",
    "contribute",
];

/// `parse_kind` accepts every `JobKind`, and the 400 names that vocabulary. A kind missing here
/// is a kind the schema tells an agent not to send.
#[test]
fn every_job_kind_door_names_contribute() {
    for id in ["enqueue_job", "create_schedule", "update_schedule"] {
        let row = ENDPOINTS.iter().find(|e| e.id == id).unwrap();
        assert_eq!(row.param("type").expect("type").enum_values, KINDS, "{id}");
    }
}
