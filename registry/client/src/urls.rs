//! The registry's two index addresses.
//!
//! These are product constants, not configuration. A user-suppliable
//! index URL would turn every `pull` into "download and execute whatever
//! this URL says".
//!
//! Only the explicit `registry-test-overrides` cargo feature compiles the
//! env overrides in. Environment inheritance is not a security boundary:
//! a service wrapper, shell profile, package hook or compromised parent
//! process could otherwise redirect a privileged `postvec model pull` to
//! an attacker-controlled index, and the archive digest would then only
//! prove consistency with that attacker's index. Without the feature
//! (every default build, debug or release) the two constants below are
//! the only origins the client will ever contact, and the full
//! HTTPS/public-destination transport policy always applies. The
//! integration test suite enables the feature explicitly
//! (`cargo test -p postvec-cli --features registry-test-overrides`), and
//! a build with it reports loudly when an override is in effect.

/// Anonymous channel: the public bucket's static index.
pub const PUBLIC_INDEX_URL: &str =
    "https://univec-registry-public.s3.eu-west-1.amazonaws.com/v1/index.json";

/// Authenticated channel: Aphex's identity-gated index route.
pub const AUTHENTICATED_INDEX_URL: &str = "https://api.univec.ai/v1/registry/index.json";

/// Where a signed-in user reviews their account: the remediation target
/// for the not-in-your-catalogue message. A compiled product constant
/// like the index addresses. Remediation text comes from the CLI, never
/// from the served index.
pub const DASHBOARD_URL: &str = "https://univec.ai/dashboard";

#[cfg(feature = "registry-test-overrides")]
const PUBLIC_OVERRIDE_ENV: &str = "POSTVEC_REGISTRY_PUBLIC_INDEX_URL";
#[cfg(feature = "registry-test-overrides")]
const AUTHENTICATED_OVERRIDE_ENV: &str = "POSTVEC_REGISTRY_AUTH_INDEX_URL";

/// The effective index URL for a channel, and whether an override is active.
///
/// `overridden` can only ever be true in a `registry-test-overrides` build.
/// An override also relaxes the transport policy (plain HTTP, loopback
/// destinations) so tests can serve fixtures — which is exactly why the
/// overrides are compiled out of every default build rather than merely
/// undocumented.
#[derive(Debug, Clone)]
pub struct IndexUrl {
    pub url: String,
    pub overridden: bool,
}

fn fixed(default: &str) -> IndexUrl {
    IndexUrl {
        url: default.to_string(),
        overridden: false,
    }
}

#[cfg(feature = "registry-test-overrides")]
fn resolve(env_name: &str, default: &str) -> IndexUrl {
    match std::env::var(env_name) {
        Ok(value) if !value.trim().is_empty() => IndexUrl {
            url: value.trim().to_string(),
            overridden: true,
        },
        _ => fixed(default),
    }
}

pub fn public_index_url() -> IndexUrl {
    #[cfg(feature = "registry-test-overrides")]
    return resolve(PUBLIC_OVERRIDE_ENV, PUBLIC_INDEX_URL);
    #[cfg(not(feature = "registry-test-overrides"))]
    fixed(PUBLIC_INDEX_URL)
}

pub fn authenticated_index_url() -> IndexUrl {
    #[cfg(feature = "registry-test-overrides")]
    return resolve(AUTHENTICATED_OVERRIDE_ENV, AUTHENTICATED_INDEX_URL);
    #[cfg(not(feature = "registry-test-overrides"))]
    fixed(AUTHENTICATED_INDEX_URL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_https_and_stable() {
        assert!(PUBLIC_INDEX_URL.starts_with("https://"));
        assert!(AUTHENTICATED_INDEX_URL.starts_with("https://"));
        // The one address that cannot move without stranding clients ends in
        // the versioned path the schema promises.
        assert!(PUBLIC_INDEX_URL.ends_with("/v1/index.json"));
        assert!(AUTHENTICATED_INDEX_URL.ends_with("/v1/registry/index.json"));
    }

    /// Any build without the explicit test feature must ignore the
    /// override variables entirely, debug or release alike. This runs
    /// under a plain `cargo test -p postvec-cli` (no features), which is
    /// also what the packaging pipeline builds.
    #[cfg(not(feature = "registry-test-overrides"))]
    #[test]
    fn builds_without_the_test_feature_ignore_the_override_environment() {
        std::env::set_var("POSTVEC_REGISTRY_PUBLIC_INDEX_URL", "http://127.0.0.1:1/x");
        std::env::set_var("POSTVEC_REGISTRY_AUTH_INDEX_URL", "http://127.0.0.1:1/y");
        let public = public_index_url();
        let auth = authenticated_index_url();
        std::env::remove_var("POSTVEC_REGISTRY_PUBLIC_INDEX_URL");
        std::env::remove_var("POSTVEC_REGISTRY_AUTH_INDEX_URL");
        assert_eq!(public.url, PUBLIC_INDEX_URL);
        assert!(!public.overridden);
        assert_eq!(auth.url, AUTHENTICATED_INDEX_URL);
        assert!(!auth.overridden);
    }
}
