use serde::{Deserialize, Serialize};

/// Reference to secret material owned by a product credential store.
///
/// This is intentionally a handle, not a token or secret value. Consumers may
/// resolve it through Vault, a database secret table, an OS keychain, or a
/// per-request session store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialRef {
    /// Store-local credential identifier.
    pub id: String,
    /// Optional stable version, revision, or generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl CredentialRef {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: None,
        }
    }

    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }
}

/// API authentication scheme required by a connection or endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum AuthScheme {
    None,
    BearerToken,
    ApiKey {
        location: ApiKeyLocation,
        name: String,
    },
    OAuth2 {
        flows: Vec<OAuth2Flow>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
    },
}

/// Location for API-key injection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ApiKeyLocation {
    Header,
    Query,
    Cookie,
}

/// Supported OAuth2 flow labels. The actual token exchange remains in the
/// product adapter because it depends on provider endpoints and policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum OAuth2Flow {
    AuthorizationCode,
    ClientCredentials,
    DeviceCode,
    RefreshToken,
}

/// Declared auth requirement for an API surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuthRequirement {
    pub scheme: AuthScheme,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

impl AuthRequirement {
    #[must_use]
    pub fn new(scheme: AuthScheme) -> Self {
        Self {
            scheme,
            scopes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }
}

/// Binding between an auth requirement and a credential reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ConnectionAuth {
    None,
    Credential {
        requirement: AuthRequirement,
        credential: CredentialRef,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_ref_carries_no_secret_value() {
        let auth = ConnectionAuth::Credential {
            requirement: AuthRequirement::new(AuthScheme::BearerToken),
            credential: CredentialRef::new("cred_oauth_openai").with_version("7"),
        };

        let json = serde_json::to_string(&auth).unwrap();
        assert!(json.contains("cred_oauth_openai"));
        assert!(!json.contains("access_token"));
        assert!(!json.contains("refresh_token"));
    }
}
