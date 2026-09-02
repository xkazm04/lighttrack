//! The webhook signature contract, as used by the *sender*.
//!
//! The implementation lives in [`lighttrack_core::alert_sign`], not here: the responder verifies
//! what this crate signs, and a signature scheme with two implementations is a scheme with two
//! behaviours. This module is the seam so the alert code reads `sign::signature_header(..)` like
//! any other neighbour — see the core module for the header format, the derived-key rule, and the
//! rotation behaviour.

pub(crate) use lighttrack_core::alert_sign::{derive_key, signature_header, SIGNATURE_HEADER};
