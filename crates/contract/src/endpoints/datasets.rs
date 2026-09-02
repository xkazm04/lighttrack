//! The evaluation corpus: datasets, their cases, freezing, and the lineage (fork /
//! import / version walk) that makes a frozen set a checkpoint rather than a dead end.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "create_dataset",
        method: Method::Post,
        path: "/v1/projects/:id/datasets",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            br("name", JsonTy::String, "dataset name; its versions share it"),
            b("source", JsonTy::String, "provenance label, e.g. manual or events:recent"),
        ],
        response: TypeRef::Named("Dataset"),
        mcp: Some(McpTool {
            name: "create_dataset",
            description: "Create a dataset in a project.",
            read_only: false,
            args: &["id", "name", "source"],
            ..McpTool::DEFAULT
        }),
        doc: "Create a dataset — version 1, unfrozen, the root of its own lineage.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_datasets",
        method: Method::Get,
        path: "/v1/projects/:id/datasets",
        access: Key(Read),
        params: &[pm("id", "project", "project id")],
        response: TypeRef::ArrayOf("Dataset"),
        mcp: Some(McpTool {
            name: "list_datasets",
            description: "List a project's datasets.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("list_datasets"),
        doc: "A project's datasets.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_dataset",
        method: Method::Get,
        path: "/v1/datasets/:id",
        access: Key(Read),
        params: &[pm("id", "dataset", "dataset id")],
        response: TypeRef::Named("Dataset"),
        mcp: Some(McpTool {
            name: "get_dataset",
            description: "Fetch one dataset by id.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("get_dataset"),
        doc: "One dataset by id; another project's is not found rather than refused.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "add_dataset_item",
        method: Method::Post,
        path: "/v1/datasets/:id/items",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "dataset", "dataset id"),
            br("input", JsonTy::String, "the prompt / case input"),
            b("output", JsonTy::String, "a captured/candidate response"),
            b("expected", JsonTy::String, "golden reference answer"),
            b("context", JsonTy::String, ""),
            b("tags", JsonTy::Array, ""),
        ],
        response: TypeRef::Named("DatasetItem"),
        mcp: Some(McpTool {
            name: "add_dataset_item",
            description: "Append a case to a (non-frozen) dataset.",
            read_only: false,
            args: &["id", "input", "output", "expected", "context", "tags"],
            ..McpTool::DEFAULT
        }),
        doc: "Append one case; 409 if the dataset is frozen.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_dataset_items",
        method: Method::Get,
        path: "/v1/datasets/:id/items",
        access: Key(Read),
        params: &[pm("id", "dataset", "dataset id")],
        response: TypeRef::ArrayOf("DatasetItem"),
        mcp: Some(McpTool {
            name: "list_dataset_items",
            description: "List the cases in a dataset.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("list_dataset_items"),
        doc: "The cases in one dataset.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "freeze_dataset",
        method: Method::Post,
        path: "/v1/datasets/:id/freeze",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[pm("id", "dataset", "dataset id")],
        response: TypeRef::Named("Dataset"),
        mcp: Some(McpTool {
            name: "freeze_dataset",
            description: "Freeze a dataset so it becomes immutable, fixing the input half of run comparability. Runs months apart are only comparable if the models under test have gained no exposure to the cases meanwhile, which imported datasets cannot guarantee. Idempotent.",
            read_only: false,
            idempotent: true,
            args: &["id"],
        }),
        doc: "Freeze a dataset so a finished run's input half stays fixed.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "dataset_item_from_label",
        method: Method::Post,
        path: "/v1/datasets/:id/items/from-label",
        access: Admin,
        mutating: true,
        params: &[
            p("id", "dataset id to promote into"),
            br("label_id", JsonTy::String, "the label to promote; its subject must be an event"),
        ],
        response: TypeRef::Named("DatasetItem"),
        cli: Some(&["datasets", "promote"]),
        doc: "Promote a labelled production event into a golden case, copying the human verdict onto it.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_dataset_labels",
        method: Method::Get,
        path: "/v1/datasets/:id/labels",
        access: Key(Read),
        params: &[p("id", "dataset id")],
        response: TypeRef::ArrayOf("Label"),
        cli: Some(&["datasets", "labels"]),
        doc: "Every human verdict on this set's items — the join `lt-runner calibrate --dataset` reads.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "fork_dataset",
        method: Method::Post,
        path: "/v1/datasets/:id/fork",
        access: Admin,
        mutating: true,
        params: &[pm("id", "dataset", "id of the dataset to fork")],
        response: TypeRef::Named("Dataset"),
        mcp: Some(McpTool {
            name: "fork_dataset",
            description: "Fork a FROZEN dataset into the next version of its name (M24): items and their human labels copied, unfrozen, parent linked. This is how a golden set is extended — freezing is a checkpoint, not a dead end, and writing to the frozen one would rewrite what a finished run was scored against. The new version's `version` is what a run's dataset_pin records, so two runs over different corpora stop comparing as if they were the same.",
            read_only: false,
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["datasets", "fork"]),
        doc: "Fork a frozen dataset into the next version of its name, labels and all.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "import_dataset_items",
        method: Method::Post,
        path: "/v1/datasets/:id/items/import",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "dataset", "id of the dataset to import into"),
            be("from", &["events", "scores"], "which table the cases come from (default events)"),
            be("strategy", &["recent", "random", "stratified", "errors"], "how to choose them (default recent)"),
            b("n", JsonTy::Integer, "how many to mine (default 50, cap 5000)"),
            b("dedupe", JsonTy::Boolean, "skip cases whose normalised input is already in the set (default false)"),
            b("below", JsonTy::Number, "with from=scores: only verdicts whose normalised value (value/max) is below this"),
            b("model", JsonTy::String, "only events from this model"),
            be("status", &["success", "error", "timeout"], "only events with this outcome"),
            b("since", JsonTy::String, "RFC3339: only events at or after this instant"),
            b("event_ids", JsonTy::Array, "import exactly these events, bypassing the filter and strategy"),
        ],
        response: TypeRef::Untyped(
            "{ dataset_id, imported } — cases actually WRITTEN, not matched: with `dedupe`, 0 is a \
             successful answer.",
        ),
        mcp: Some(McpTool {
            name: "import_dataset_items",
            description: "Mine stored rows into an UNFROZEN dataset (M24). Strategies: recent (newest), random (uniform over what matched), stratified (a per model+status quota, so a low-volume model is represented rather than drowned), errors (failures only). `from: scores` joins verdicts, which is what makes a failure-mined regression set possible. Mined production text is scrubbed on the way in. Returns how many cases were WRITTEN — with dedupe, a near-duplicate of a case already in the set is not, and 0 is a successful answer. 409 if the target is frozen: fork it first.",
            read_only: false,
            args: &["id", "from", "strategy", "n", "dedupe", "below", "model", "status", "since", "event_ids"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["datasets", "import"]),
        doc: "Mine stored events or verdicts into an unfrozen dataset; 409 if it is frozen.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_dataset_versions",
        method: Method::Get,
        path: "/v1/projects/:id/datasets/versions",
        access: Key(Read),
        params: &[
            p("id", "project id"),
            qr("name", "the dataset name to walk — a query parameter because a name routinely contains `/`"),
        ],
        response: TypeRef::ArrayOf("Dataset"),
        cli: Some(&["datasets", "versions"]),
        doc: "Every version of one dataset name, newest first — what a run's dataset_pin resolves to.",
        ..Endpoint::DEFAULT
    },
];
