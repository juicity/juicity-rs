/// Build-time information injected by `build.rs`.
pub struct BuildInfo;

impl BuildInfo {
    /// Package version (e.g. `"0.1.0"`)
    pub const PKG_VERSION: &'static str = env!("CARGO_PKG_VERSION");

    /// Build timestamp in ISO 8601 format (set by `build.rs`)
    pub const BUILD_TIMESTAMP: &'static str = env!("JUICITY_BUILD_TIMESTAMP");

    /// Short Git commit hash (set by `build.rs`), falls back to `"unknown"`
    pub const GIT_HASH: &'static str = env!("JUICITY_GIT_HASH");

    /// Formatted version string suitable for `--version` output.
    /// Clap prefixes the binary name automatically, so we start with just the version number.
    pub const fn version_string() -> &'static str {
        // `concat!` only accepts string literals, so we inline `env!()` calls here.
        concat!(
            "v",
            env!("CARGO_PKG_VERSION"),
            "\nbuild time: ",
            env!("JUICITY_BUILD_TIMESTAMP"),
            "\ncommit: ",
            env!("JUICITY_GIT_HASH"),
        )
    }
}
