//! Test support for craft-live: a fake Nextcloud ([`fake`]) and a y-websocket client that plays
//! a craft app's web build ([`yclient`]).
// Test code: a broken fixture should stop the test loudly.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::result_large_err)]

pub mod fake;
pub mod yclient;

pub use fake::FakeNextcloud;
pub use yclient::{WFrame, YClient};
