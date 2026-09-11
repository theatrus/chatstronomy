//! The centralized Chatstronomy hub service.
//!
//! The Hub accepts outbound Direct connections from N.I.N.A. plugins. One
//! hosted process owns the central Discord application, a
//! web app for login and telescope management, and the `/v1/direct`
//! WebSocket listener that N.I.N.A. plugins connect to.
//! All durable state lives in one SQLite database.
//!
//! See `docs/HOSTED_SERVICE.md` for the full design.

pub mod auth;
pub mod autofocus_delivery;
pub mod config;
pub mod db;
pub mod direct_server;
pub mod direct_source;
pub mod discord_api;
pub mod guild_check;
pub mod rate_limit;
pub mod rig_resolver;
pub mod server;
pub mod store;
pub mod tenants;
pub mod updaters;
pub mod web_ui;
