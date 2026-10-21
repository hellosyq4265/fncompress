//! fncompress server (M2): SQLite store, task scheduler, directory
//! monitoring and the HTTP API behind `fncompress serve`, plus the embedded
//! front end (M3).

pub mod api;
pub mod app;
pub mod browse;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod janitor;
pub mod model;
pub mod monitor;
pub mod scan;
pub mod scheduler;
pub mod server;
pub mod space;
pub mod web;
