//! Product-neutral browser-suite navigation contract.
//!
//! Products own their routes and domain navigation. A deployment may expose one
//! trusted hub destination so separately delivered consoles can return to the
//! suite without learning account, tenant, billing, or product topology.

use serde::{Deserialize, Serialize};

/// Same-origin endpoint serving [`SuiteNavigation`].
pub const SUITE_NAVIGATION_PATH: &str = "/.well-known/awaken-suite-navigation";

/// Optional deployment-owned navigation out of one product console.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SuiteNavigation {
    /// Exact trusted URL of the suite hub. `None` keeps a standalone product
    /// self-contained and must not be replaced by hostname inference.
    pub hub_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause-effect graph:
    /// deployment hub configured -> serialize exact opaque destination;
    /// deployment hub absent -> serialize an explicit null capability;
    /// extra wire field -> reject instead of silently accepting contract drift.
    ///
    /// Decision table:
    /// | rule | hub input | unknown field | effect |
    /// | R1 | exact URL | no | exact round trip |
    /// | R2 | absent | no | explicit standalone projection |
    /// | R3 | any | yes | decode failure |
    #[test]
    fn suite_navigation_wire_is_exact_and_fail_closed() {
        let hosted = SuiteNavigation {
            hub_url: Some("https://cloud.example/products".to_owned()),
        };
        let encoded = serde_json::to_string(&hosted).expect("serialize hosted navigation");
        assert_eq!(
            serde_json::from_str::<SuiteNavigation>(&encoded).unwrap(),
            hosted,
            "R1"
        );

        let standalone = serde_json::to_value(SuiteNavigation::default()).unwrap();
        assert_eq!(standalone, serde_json::json!({ "hub_url": null }), "R2");

        assert!(
            serde_json::from_value::<SuiteNavigation>(serde_json::json!({
                "hub_url": null,
                "tenant_id": "must-not-cross-this-contract"
            }))
            .is_err(),
            "R3"
        );
    }
}
