//! Postgres-specific store helper: sqlx error mapping. The timestamp/enum/JSON codecs are shared
//! across all backends and re-exported here so the per-domain modules import them alongside `pgerr`
//! from one place — see [`lighttrack_store::codec`].

use lighttrack_store::StoreError;

pub(crate) use lighttrack_store::codec::{
    enum_to_str, fmt_ts, json_or_null, parse_enum, parse_ts, val_or_null,
};

pub(crate) fn pgerr(e: sqlx::Error) -> StoreError {
    StoreError::Other(format!("postgres: {e}"))
}
