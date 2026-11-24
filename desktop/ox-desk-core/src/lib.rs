//! Data shared by the OX desktop workbench and its native services.
#[path = "../../../src/settings_protocol.rs"]
pub mod settings_protocol;

pub mod actions;
pub mod asks;
pub mod changed;
pub mod claude;
pub mod codex;
pub mod desktop_behavior;
pub mod host_protocol;
pub mod inbox;
pub mod links;
pub mod perf;
pub mod ports;
pub mod slash;
pub mod store;
pub mod transcript;

pub mod keys;
pub mod worker;

pub mod mentions;
pub mod native_catalog;
pub mod usage;

pub mod runtime;

pub mod titles;

pub mod cartridge;
