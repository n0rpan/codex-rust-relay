/// The version this binary reports. Release builds set `OX_BUILD_VERSION`
/// at compile time (for example `0.1.1-dev.38`); every other build uses the
/// version in Cargo.toml.
pub(crate) const VERSION: &str = match option_env!("OX_BUILD_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};
