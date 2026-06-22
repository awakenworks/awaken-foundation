//! Namespaced, layered configuration so services compose without key collisions.
//!
//! The goal is "separable and combinable" services: each service runs standalone
//! *or* composed into one deployment, and combining them must never produce a
//! configuration conflict. Two rules make that hold:
//!
//! - **Namespace by service.** Every service owns a top-level namespace
//!   (`iam`, `region`, `billing`, …) and reads only its own [`Section`]. Two
//!   services never share a key, so a combined deployment is just several
//!   namespaced sections side by side — the same discipline IAM already applies
//!   to database tables via a `with_prefix` table prefix, generalised to config.
//! - **Deterministic precedence.** Values are layered in order (defaults, then a
//!   file, then the environment, then flags); a later [`Config::layer`] overrides
//!   an earlier one for the same key. The composition root loads the layers once
//!   and hands each service its [`Section`], so there is no global mutable config
//!   the services fight over.
//!
//! ```
//! use awaken_config::Config;
//! let mut cfg = Config::new();
//! cfg.layer([("iam.issuer", "default"), ("region.id", "eu")]);   // defaults
//! cfg.layer([("iam.issuer", "https://iam.example")]);            // file overrides
//! let iam = cfg.section("iam");
//! assert_eq!(iam.get("issuer"), Some("https://iam.example"));
//! assert_eq!(cfg.section("region").get("id"), Some("eu"));
//! assert!(iam.get("missing").is_none());
//! ```

use std::collections::BTreeMap;

/// Configuration error surfaced when a required key is absent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A required key was not present in the resolved configuration.
    #[error("missing required config key `{0}`")]
    Missing(String),
}

/// A layered configuration. Each layer is a flat map of fully-qualified
/// `"<namespace>.<key>"` entries; later layers override earlier ones.
#[derive(Debug, Default, Clone)]
pub struct Config {
    resolved: BTreeMap<String, String>,
}

impl Config {
    /// An empty configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a layer. Entries override any earlier value for the same key, so
    /// callers layer from lowest precedence (defaults) to highest (flags).
    pub fn layer<I, K, V>(&mut self, entries: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (key, value) in entries {
            self.resolved.insert(key.into(), value.into());
        }
        self
    }

    /// View the section a single service owns. Keys inside are read relative to
    /// the namespace, so a service never sees — or collides with — another's.
    pub fn section<'a>(&'a self, namespace: &str) -> Section<'a> {
        Section {
            config: self,
            prefix: format!("{namespace}."),
        }
    }
}

/// A read view scoped to one namespace. Constructed via [`Config::section`].
pub struct Section<'a> {
    config: &'a Config,
    prefix: String,
}

impl Section<'_> {
    /// The resolved value for `key` within this section, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.config
            .resolved
            .get(&format!("{}{key}", self.prefix))
            .map(String::as_str)
    }

    /// The resolved value for `key`, or [`ConfigError::Missing`] (fail closed:
    /// an absent required key is an error, never a silent default).
    pub fn require(&self, key: &str) -> Result<&str, ConfigError> {
        self.get(key)
            .ok_or_else(|| ConfigError::Missing(format!("{}{key}", self.prefix)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_isolate_services() {
        let mut cfg = Config::new();
        cfg.layer([("iam.issuer", "a"), ("region.issuer", "b")]);
        // Same bare key `issuer` in two services never collides.
        assert_eq!(cfg.section("iam").get("issuer"), Some("a"));
        assert_eq!(cfg.section("region").get("issuer"), Some("b"));
    }

    #[test]
    fn later_layers_win() {
        let mut cfg = Config::new();
        cfg.layer([("iam.issuer", "default")]);
        cfg.layer([("iam.issuer", "file")]);
        cfg.layer([("iam.issuer", "env")]);
        assert_eq!(cfg.section("iam").get("issuer"), Some("env"));
    }

    #[test]
    fn require_fails_closed() {
        let cfg = Config::new();
        assert_eq!(
            cfg.section("iam").require("issuer"),
            Err(ConfigError::Missing("iam.issuer".into()))
        );
    }
}
