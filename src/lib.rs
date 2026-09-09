//! `trigrep` — serverless trigram-indexed grep (the `tg` binary).
//!
//! One SQLite-format cache file per indexed root, maintained through
//! `db-storage`'s b-tree/pager layer; every invocation opens it, brings it
//! up to date, queries it and exits. No daemon (contrast microsoft/tgrep),
//! no SQL (the query plan is a `TableCursor::seek` per trigram). See
//! `.openspec/adr/0001-serverless-trigram-cache.md`.
#![forbid(unsafe_code)]

pub mod cache;
pub mod codec;
pub mod index;
pub mod search;
pub mod walk;
