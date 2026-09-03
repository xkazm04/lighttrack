use thiserror::Error;

/// Errors surfaced by core logic. One variant, because one thing in this crate is fallible: parsing
/// a price book. The enum had three more (`UnknownModel`, `Serde`, `Other`) that nothing anywhere
/// constructed — a `match` on this type had to name arms that could never be taken, and the doc
/// promised wrapping in "service crates' own error types" that none of them did.
#[derive(Debug, Error)]
pub enum LtError {
    #[error("invalid price book: {0}")]
    InvalidPriceBook(String),
}

pub type Result<T> = std::result::Result<T, LtError>;
