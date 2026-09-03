//! The two doors that describe the deployment rather than its data, and the only two outside
//! `/v1`: liveness, and the generated OpenAPI document.

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
