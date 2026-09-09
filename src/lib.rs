//! `trigrep` — serverless trigram-indexed grep (the `tg` binary).
//!
//! One SQLite-format cache file per indexed root, maintained through
//! `db-storage`'s b-tree/pager layer; every invocation opens it, brings it
//! up to date, queries it and exits. No daemon (contrast microsoft/tgrep),
//! no SQL (the query plan is a `TableCursor::seek` per trigram). See
//! `.openspec/adr/0001-serverless-trigram-cache.md`.
#![forbid(unsafe_code)]
// The panic lints are a production rule; inline tests fail fast (see
// clippy.toml and Cargo.toml [lints.clippy]).
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::indexing_slicing
    )
)]

pub mod cache;
pub mod codec;
pub mod index;
pub mod search;
pub mod walk;
