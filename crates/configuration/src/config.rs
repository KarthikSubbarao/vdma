//! The root `Configuration` and TOML parsing, depending on the leaves for their local sections.

use crate::jemalloc::JemallocConfiguration;
use crate::observability::ObservabilityConfiguration;

/// Re-export of the libfabric DMA section so callers configure it through one crate.
pub use dma_libfabric::Configuration as DmaLibfabricConfiguration;

/// Module configuration root.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// `[dma-libfabric]`.
    #[serde(default, rename = "dma-libfabric")]
    pub dma_libfabric: DmaLibfabricConfiguration,
    /// `[jemalloc]`.
    #[serde(default)]
    pub jemalloc: JemallocConfiguration,
    /// `[observability]`.
    #[serde(default)]
    pub observability: ObservabilityConfiguration,
}

impl Configuration {
    /// Parse a configuration from a TOML document.
    pub fn from_toml(document: &str) -> Result<Self, ConfigurationError> {
        toml::from_str(document).map_err(|error| ConfigurationError::Parse(error.to_string()))
    }
}

/// Errors loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("failed to parse configuration: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use dma_libfabric::Provider;

    use super::Configuration;

    #[test]
    fn empty_document_uses_defaults() {
        let configuration = Configuration::from_toml("").expect("defaults parse");
        assert_eq!(configuration, Configuration::default());
        assert_eq!(configuration.dma_libfabric.providers, vec![Provider::Tcp]);
    }

    /// A misspelled or retired key must fail the load rather than be dropped. Silently ignoring one
    /// costs a debugging session: the operator sets a knob, sees no effect, and blames the code.
    #[test]
    fn rejects_unknown_keys() {
        // A retired key, at the section level.
        let error = Configuration::from_toml("[dma-libfabric]\ndirection = \"bidirectional\"\n")
            .expect_err("retired key rejected");
        assert!(
            error.to_string().contains("direction"),
            "error should name the offending key, got: {error}"
        );
        // A hyphenated spelling of a snake_case key.
        assert!(
            Configuration::from_toml("[dma-libfabric]\ncrc-pool-threads = 8\n").is_err(),
            "hyphenated spelling of crc_pool_threads should be rejected"
        );
        // And at the root and the nested observability section.
        assert!(Configuration::from_toml("nonsense = 1\n").is_err());
        assert!(
            Configuration::from_toml("[observability.console-host]\nlisten_addr = \"1.2.3.4:1\"\n")
                .is_err()
        );
    }

    #[test]
    fn example_configs_parse_with_console_host_enabled() {
        let examples = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/");
        for name in ["tcp", "efa"] {
            let document =
                std::fs::read_to_string(format!("{examples}{name}.toml")).expect("read example");
            let configuration = Configuration::from_toml(&document).expect("example parses");
            assert!(
                configuration.observability.console_host.is_some(),
                "{name}.toml should enable the console host"
            );
        }
    }

    #[test]
    fn console_host_is_disabled_by_default() {
        let configuration = Configuration::from_toml("").expect("defaults parse");
        assert!(configuration.observability.console_host.is_none());
    }

    #[test]
    fn parses_console_host_section() {
        let document = r#"
            [observability.console-host]
            listen = "0.0.0.0:9999"
        "#;
        let configuration = Configuration::from_toml(document).expect("section parses");
        let console_host = configuration
            .observability
            .console_host
            .expect("console host enabled");
        assert_eq!(console_host.listen, "0.0.0.0:9999".parse().expect("addr"));
    }

    #[test]
    fn parses_jemalloc_section() {
        let document = r#"
            [jemalloc]
            huge_arena_decay_ms = -1
        "#;
        let configuration = Configuration::from_toml(document).expect("section parses");
        assert_eq!(configuration.jemalloc.huge_arena_decay_ms, -1);
        // Unset keys keep their defaults rather than zeroing.
        assert_eq!(
            configuration.jemalloc.huge_arena_oversize_threshold,
            Configuration::default()
                .jemalloc
                .huge_arena_oversize_threshold
        );
        // The knobs left [dma-libfabric]; a config still setting them there must fail loudly.
        assert!(Configuration::from_toml("[dma-libfabric]\nhuge_arena_decay_ms = 1000\n").is_err());
    }

    #[test]
    fn parses_libfabric_section() {
        let document = r#"
            [dma-libfabric]
            providers = ["efa-direct", "tcp"]
            interfaces = ["rdmap83s0-rdm"]
            bind = "10.0.0.1"
        "#;
        let configuration = Configuration::from_toml(document).expect("section parses");
        assert_eq!(
            configuration.dma_libfabric.providers,
            vec![Provider::EfaDirect, Provider::Tcp]
        );
        assert_eq!(
            configuration.dma_libfabric.interfaces,
            vec!["rdmap83s0-rdm"]
        );
        assert_eq!(
            configuration.dma_libfabric.bind.as_deref(),
            Some("10.0.0.1")
        );
        assert!(!configuration.dma_libfabric.quiesce_writes);
    }

    #[test]
    fn parses_quiesce_writes() {
        let configuration = Configuration::from_toml("[dma-libfabric]\nquiesce_writes = true\n")
            .expect("section parses");
        assert!(configuration.dma_libfabric.quiesce_writes);
    }
}
