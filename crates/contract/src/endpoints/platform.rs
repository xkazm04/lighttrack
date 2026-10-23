//! The doors that describe the deployment rather than its data, and the only ones outside `/v1`:
//! the operator's health rollup, the two orchestrator probes, and the generated OpenAPI document.

use crate::types::*;
use Access::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "health",
        method: Method::Get,
        path: "/health",
        // Unauthenticated on purpose: a liveness probe that needs a credential is a liveness probe
        // an orchestrator cannot run, and this answers nothing about the tenant's data.
        access: Unauthenticated,
        response: TypeRef::Untyped(
            "{ status, backend, surfaces: [name] } — liveness plus the store backend's declared \
             surfaces.",
        ),
        cli: Some(&["health"]),
        doc: "Liveness, and the store backend's declared surfaces.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "health_live",
        method: Method::Get,
        path: "/health/live",
        // Declared, not merely routed. The router has always served this; the contract had not
        // heard of it, which was survivable while authorization was a string comparison over
        // `main.rs` and became an outage the moment one layer resolved every request against this
        // table: an undeclared pair is a 500, and a 500 on a liveness probe is a restart loop.
        access: Unauthenticated,
        response: TypeRef::Untyped("{ status: \"ok\" } — the process answered, nothing else."),
        machine: true,
        doc: "Liveness: is this process wedged. Observes nothing outside itself.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "health_ready",
        method: Method::Get,
        path: "/health/ready",
        access: Unauthenticated,
        response: TypeRef::Untyped("{ status, checks } — 200 when this pod should serve."),
        machine: true,
        doc: "Readiness: should traffic arrive. Observes the real store.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "openapi",
        method: Method::Get,
        path: "/openapi.json",
        // Also unauthenticated: it describes the shape of the API, never a row of anyone's data,
        // and a client generator that had to hold a key to read it would be one more secret in one
        // more CI job.
        access: Unauthenticated,
        response: TypeRef::Untyped(
            "An OpenAPI 3.1 document generated from this contract: every path, its parameters, its \
             request and response schemas, and the capability each operation requires.",
        ),
        cli: Some(&["openapi"]),
        doc: "This deployment's OpenAPI 3.1 description, generated from the endpoint contract.",
        ..Endpoint::DEFAULT
    },
];
