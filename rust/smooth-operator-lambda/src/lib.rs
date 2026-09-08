//! The API Gateway WebSocket Lambda, as a library.
//!
//! `src/main.rs` is the `bootstrap` binary AWS runs; this exposes the same
//! modules so the protocol path can be driven from integration tests without
//! deploying anything. See `tests/protocol_smoke.rs`, which runs real frames
//! through [`dispatch::handle_frame`] against DynamoDB Local and reads the
//! replies out of a [`poster::ConnectionPoster::Capturing`].
//!
//! Keep this in sync with the `mod` list in `main.rs` — the binary declares its
//! own modules, so a module added there is not automatically available here.

pub mod adapter;
pub mod config;
pub mod connection;
pub mod dispatch;
pub mod poster;
