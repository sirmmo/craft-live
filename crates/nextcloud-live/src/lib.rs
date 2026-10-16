//! nextcloud-live: live co-editing of Nextcloud files for the craft apps. A craft plugin
//! protocol 2 transport that joins craft-live rooms over y-websocket (Appendix B), so people in
//! GridCraft, WordCraft and DeckCraft edit a Nextcloud file together with the people who have it
//! open in Nextcloud Office.
//!
//! - [`runtime`]: the push channel, the rooms, `collab/*` ↔ y-websocket, saves.
//! - [`link`]: a room's WebSocket with reconnects.
//! - [`commands`]: Open, Share, Join, Leave, Status, Connect, Settings.
//! - [`net`], [`store`], [`wire`]: HTTP to Nextcloud and craft-live, the data folder, framing.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable, clippy::indexing_slicing)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing))]
#![forbid(unsafe_code)]

pub mod commands;
pub mod link;
pub mod manifest;
pub mod net;
pub mod runtime;
pub mod store;
pub mod wire;
