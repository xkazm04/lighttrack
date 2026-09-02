//! The human verdict ledger (M11): labels, the judge-human calibration series, and the
//! trust verdict a green benchmark badge should be read against.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "post_label",
        method: Method::Post,
        path: "/v1/labels",
        access: Key(Manage),
        mutating: true,
        params: &[
            br("subject", JsonTy::String, "'<kind>:<id>' with kind one of event, dataset_item, score"),
            br("value", JsonTy::Number, "overall quality 0-1, on the same scale a judge verdict normalizes to"),
            br("labeler", JsonTy::String, "who said so — a person, a team"),
            b("project_id", JsonTy::String, "project id (required with an admin key)"),
            b("pass", JsonTy::Boolean, "an explicit human pass/fail; omit to derive it from `value`"),
            b("rubric_id", JsonTy::String, "the rubric this opinion was formed under, if any"),
            b("dimensions", JsonTy::Array, "per-dimension human scores, when the grade was structured"),
            b("note", JsonTy::String, ""),
        ],
        response: TypeRef::Named("Label"),
        mcp: Some(McpTool {
            name: "record_label",
            description: "Record one human verdict (M11) — the ground truth a judge is calibrated against. `labeler` is required: a verdict with no attribution cannot be audited, which is how a calibration result becomes a number nobody can defend.",
            read_only: false,
            args: &["project_id", "subject", "value", "pass", "rubric_id", "labeler", "note"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["labels", "add"]),
        doc: "Record one human verdict; `labeler` is required so a calibration stays auditable.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_labels",
        method: Method::Get,
        path: "/v1/labels",
        access: Key(Read),
        paged: true,
        params: &[
            q("project", ""),
            q("subject", "'<kind>:<id>' with kind one of event, dataset_item, score"),
            q("rubric_id", ""),
            qt("limit", JsonTy::Integer, ""),
            q("cursor", "opaque keyset cursor from a previous page"),
        ],
        // `paged`, though the cursor rides in the body rather than `X-Next-Cursor`: `cursor=` is
        // still how the next page is asked for, and that is what a caller has to know.
        response: TypeRef::Untyped(
            "{ labels: [Label], next_cursor } — the cursor rides in the body here, not a header.",
        ),
        mcp: Some(McpTool {
            name: "list_labels",
            description: "Human verdicts (M11): what a person said about an event, a golden-set item, or a judge's own verdict — the ground truth a judge is calibrated against. Narrow with `subject` (`event:<id>` / `dataset_item:<id>` / `score:<id>`) or `rubric_id`.",
            args: &["project", "subject", "rubric_id", "limit", "cursor"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["labels", "list"]),
        render_kind: Some("list_labels"),
        doc: "The human verdict ledger, newest first.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "post_calibration",
        method: Method::Post,
        path: "/v1/calibrations",
        access: Key(Manage),
        mutating: true,
        body: Some(TypeRef::Named("CalibrationRecord")),
        response: TypeRef::Named("CalibrationRecord"),
        cli: Some(&["judges", "calibrate"]),
        doc: "Record a completed judge-human calibration — the row a trust verdict is decided from.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_calibrations",
        method: Method::Get,
        path: "/v1/calibrations",
        access: Key(Read),
        paged: true,
        params: &[
            q("project", ""),
            qt("limit", JsonTy::Integer, ""),
            q("cursor", "opaque keyset cursor from a previous page"),
        ],
        response: TypeRef::Untyped(
            "{ calibrations: [CalibrationRecord], next_cursor } — κ, Pearson, MAE, RMSE and n per \
             measurement, newest first.",
        ),
        mcp: Some(McpTool {
            name: "list_calibrations",
            description: "A project's judge-human calibration history, newest first — the series a drift check reads.",
            args: &["project", "limit", "cursor"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["judges", "history"]),
        render_kind: Some("list_calibrations"),
        doc: "Judge-human calibration history, newest first.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_judge_trust",
        method: Method::Get,
        path: "/v1/judges/trust",
        access: Key(Read),
        params: &[
            q("project", ""),
            qr("judge", "the judge model, e.g. anthropic/claude-haiku-4-5"),
            q("rubric_id", "omit for the freeform (rubric-less) judge; a rubric never inherits that trust"),
        ],
        response: TypeRef::Untyped(
            "{ trust: trusted|untrusted|unknown, calibration: CalibrationRecord? } — `unknown` is \
             'never measured', never 'failed'.",
        ),
        mcp: Some(McpTool {
            name: "get_judge_trust",
            description: "Whether a judge may be believed for a rubric: `trusted` | `untrusted` | `unknown`, with the calibration record that decided it. `unknown` is NOT `untrusted` — a judge nobody has measured has taken no check, not failed one. Ask this before reading a benchmark gate's green badge as evidence.",
            args: &["project", "judge", "rubric_id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["judges", "trust"]),
        render_kind: Some("get_judge_trust"),
        doc: "Whether one judge may be believed for one rubric, and the record that decided it.",
        ..Endpoint::DEFAULT
    },
];
