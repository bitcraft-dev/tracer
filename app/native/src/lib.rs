//! The Rust half of Tracer's window: what its pages reach as `native::<module>`.
//!
//! The window holds no state of its own about requests. It is a client of the
//! workspace's core, like the CLI and agents, and draws the core's event
//! stream; see `docs/PLAN.md`.

pub mod tracer;
