//! The Rust half of the Phase 0 probes: what the probe pages reach as
//! `native::<module>`. Each module backs one question Tracer has to answer
//! about Rux before the real app is built. See `probes/PROBES.md`.

pub mod chart;
pub mod clock;
pub mod events;
pub mod http;
pub mod json;
pub mod proc;
