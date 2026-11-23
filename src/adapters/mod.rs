//! Pure protocol adapters used by the relay's translated wires.
//!
//! Each module is self-contained and is also compiled directly by its
//! integration test (`tests/anthropic_adapter.rs`,
//! `tests/chat_completions_adapter.rs`).

pub mod anthropic;
pub mod chat_completions;
