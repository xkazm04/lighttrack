//! Terse constructors for the endpoint table.
//!
//! A hundred-and-thirty-row table written as full struct literals is unreadable, and an unreadable
//! table is one nobody checks against the router. These `const fn`s carry the defaults so a row
//! states only what is true of *it*; anything they do not cover is written out with `..Param::DEFAULT`.

use crate::types::{JsonTy, Param, ParamKind};

/// A required `:segment` of the path, named the same over MCP.
pub const fn p(name: &'static str, doc: &'static str) -> Param {
    Param {
        name,
        kind: ParamKind::Path,
        required: true,
        doc,
        ..Param::DEFAULT
    }
}

/// A required `:segment` that an MCP tool exposes under a different, pinned argument name.
pub const fn pm(name: &'static str, mcp_name: &'static str, doc: &'static str) -> Param {
    Param {
        name,
        kind: ParamKind::Path,
        required: true,
        doc,
        mcp_name: Some(mcp_name),
        ..Param::DEFAULT
    }
}

/// An optional string query parameter.
pub const fn q(name: &'static str, doc: &'static str) -> Param {
    Param {
        name,
        doc,
        ..Param::DEFAULT
    }
}

/// A required string query parameter.
pub const fn qr(name: &'static str, doc: &'static str) -> Param {
    Param {
        name,
        required: true,
        doc,
        ..Param::DEFAULT
    }
}

/// An optional query parameter of a non-string type.
pub const fn qt(name: &'static str, ty: JsonTy, doc: &'static str) -> Param {
    Param {
        name,
        ty,
        doc,
        ..Param::DEFAULT
    }
}

/// An optional query parameter with a closed value set.
pub const fn qe(
    name: &'static str,
    enum_values: &'static [&'static str],
    doc: &'static str,
) -> Param {
    Param {
        name,
        doc,
        enum_values,
        ..Param::DEFAULT
    }
}

/// An optional request-body field.
pub const fn b(name: &'static str, ty: JsonTy, doc: &'static str) -> Param {
    Param {
        name,
        kind: ParamKind::Body,
        ty,
        doc,
        ..Param::DEFAULT
    }
}

/// A required request-body field.
pub const fn br(name: &'static str, ty: JsonTy, doc: &'static str) -> Param {
    Param {
        name,
        kind: ParamKind::Body,
        ty,
        required: true,
        doc,
        ..Param::DEFAULT
    }
}

/// An optional request-body field with a closed value set.
pub const fn be(
    name: &'static str,
    enum_values: &'static [&'static str],
    doc: &'static str,
) -> Param {
    Param {
        name,
        kind: ParamKind::Body,
        enum_values,
        doc,
        ..Param::DEFAULT
    }
}

/// A required request-body field with a closed value set.
pub const fn ber(
    name: &'static str,
    enum_values: &'static [&'static str],
    doc: &'static str,
) -> Param {
    Param {
        name,
        kind: ParamKind::Body,
        required: true,
        enum_values,
        doc,
        ..Param::DEFAULT
    }
}
