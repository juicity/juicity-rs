//! Build-time versions of the GUI and its embedded cores, shown in About.

/// Versions reported in About.  The dependency versions are injected by
/// `build.rs` from `Cargo.lock`, so they cannot drift from the linked crates.
pub struct Versions {
    /// Version of the GUI itself.
    pub app: &'static str,
    /// Release tag the binary was built from.
    pub tag: &'static str,
    /// Commit the binary was built from.
    pub commit: &'static str,
    /// Version of the embedded `shadowsocks-service` (Shadowsocks-Rust).
    pub shadowsocks: &'static str,
    /// Version of the embedded `juicity-client`.
    pub juicity: &'static str,
}

impl Versions {
    pub const fn current() -> Self {
        Self {
            app: env!("CARGO_PKG_VERSION"),
            tag: juicity_common::BuildInfo::GIT_TAG,
            commit: juicity_common::BuildInfo::GIT_HASH,
            shadowsocks: env!("JUICITY_DEPS_SHADOWSOCKS_SERVICE"),
            juicity: env!("JUICITY_DEPS_JUICITY_CLIENT"),
        }
    }
}

/// Abbreviate a commit hash the way `git rev-parse --short` does.
pub fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

#[cfg(test)]
mod tests {
    use super::{short_commit, Versions};

    #[test]
    fn reports_build_time_versions() {
        let versions = Versions::current();
        for value in [
            versions.app,
            versions.tag,
            versions.commit,
            versions.shadowsocks,
            versions.juicity,
        ] {
            assert!(!value.is_empty(), "every version field must be populated");
        }
    }

    #[test]
    fn abbreviates_commits() {
        assert_eq!(
            short_commit("4c4f9f0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f"),
            "4c4f9f0"
        );
        assert_eq!(short_commit("unknown"), "unknown");
        assert_eq!(short_commit("abc"), "abc");
    }
}
