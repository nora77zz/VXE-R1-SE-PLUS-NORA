//! A small, host-side implementation of the Logitech G-series Lua API subset
//! selected for the ATK HUB reconstruction.
//!
//! This crate supplies a single Lua 5.4 worker per explicitly started script.
//! It has no Windows input implementation: applications provide an
//! [`InputBackend`] and can use a mock during development.

mod runtime;

pub use runtime::{
    BackendError, GHubRuntime, InputBackend, InputCapabilities, KeyCode, MousePosition,
    RuntimeConfig, RuntimeError, ScreenRect, ScriptEvent, ScriptExit, ScriptHandle,
    WorkerReport,
};
