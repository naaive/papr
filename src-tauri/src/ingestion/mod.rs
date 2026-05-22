//! Ingestion: fetching feeds over HTTP, parsing them, and the refresh scheduler.

pub mod discovery;
pub mod fetch;
pub mod newsletter;
pub mod parse;
pub mod rsshub;
pub mod scheduler;
pub mod sources;
