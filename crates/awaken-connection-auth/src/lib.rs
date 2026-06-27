//! Opaque caller-owned handshake material for connection transports.
//!
//! This crate carries already-materialized material that a caller has resolved
//! above the connection layer. It does not access vaults, refresh OAuth tokens,
//! choose credentials, authorize usage, persist material, or log secret values.

use std::fmt;

use thiserror::Error;

/// Errors raised while constructing header-based handshake material.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthMaterialError {
    /// A header name was empty.
    #[error("header name must not be empty")]
    EmptyHeaderName,
    /// A header name contains a byte that cannot appear in an HTTP field name.
    #[error("unsafe header name: {0:?}")]
    UnsafeHeaderName(String),
    /// A header value contains a control byte.
    #[error("unsafe header value for {0:?}")]
    UnsafeHeaderValue(String),
    /// A bearer token was empty after trimming.
    #[error("bearer token must not be empty")]
    EmptyBearerToken,
    /// A bearer token contains a control byte.
    #[error("bearer token contains a control byte")]
    UnsafeBearerToken,
}

/// Return whether `name` is a safe HTTP header field name.
#[must_use]
pub fn header_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| !byte.is_ascii_control() && byte != b' ' && byte != b':')
}

/// Return whether `value` is safe to place in an HTTP header field value.
#[must_use]
pub fn header_value_is_safe(value: &str) -> bool {
    !value.bytes().any(|byte| byte.is_ascii_control())
}

fn validate_header(name: &str, value: &str) -> Result<(), AuthMaterialError> {
    if name.is_empty() {
        return Err(AuthMaterialError::EmptyHeaderName);
    }
    if !header_name_is_safe(name) {
        return Err(AuthMaterialError::UnsafeHeaderName(name.to_string()));
    }
    if !header_value_is_safe(value) {
        return Err(AuthMaterialError::UnsafeHeaderValue(name.to_string()));
    }
    Ok(())
}

/// One already-materialized header pair.
#[derive(Clone, PartialEq, Eq)]
pub struct HeaderPair {
    name: String,
    value: String,
}

impl HeaderPair {
    /// Build a safe header pair.
    pub fn new(
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, AuthMaterialError> {
        let name = name.into();
        let value = value.into();
        validate_header(&name, &value)?;
        Ok(Self { name, value })
    }

    /// Header name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Header value. This may be secret material; do not log it.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    fn into_tuple(self) -> (String, String) {
        (self.name, self.value)
    }
}

impl fmt::Debug for HeaderPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeaderPair")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// Header-based handshake material.
///
/// Values may contain bearer/API-token material. Construction rejects header
/// injection characters and `Debug` redacts values by construction.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct HeaderAuthMaterial {
    headers: Vec<HeaderPair>,
}

impl HeaderAuthMaterial {
    /// No header material.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Build material from already-resolved headers.
    pub fn from_headers<I, N, V>(headers: I) -> Result<Self, AuthMaterialError>
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<String>,
        V: Into<String>,
    {
        let headers = headers
            .into_iter()
            .map(|(name, value)| HeaderPair::new(name, value))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { headers })
    }

    /// Build `Authorization: Bearer <token>` material.
    pub fn bearer(token: impl Into<String>) -> Result<Self, AuthMaterialError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(AuthMaterialError::EmptyBearerToken);
        }
        if !header_value_is_safe(&token) {
            return Err(AuthMaterialError::UnsafeBearerToken);
        }
        Self::from_headers([("Authorization".to_string(), format!("Bearer {token}"))])
    }

    /// Add a header pair.
    pub fn with_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, AuthMaterialError> {
        self.headers.push(HeaderPair::new(name, value)?);
        Ok(self)
    }

    /// Whether this material carries no headers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// Borrow header pairs.
    #[must_use]
    pub fn pairs(&self) -> &[HeaderPair] {
        &self.headers
    }

    /// Clone header pairs into transport-ready tuples.
    #[must_use]
    pub fn headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .cloned()
            .map(HeaderPair::into_tuple)
            .collect()
    }
}

impl fmt::Debug for HeaderAuthMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeaderAuthMaterial")
            .field("headers", &self.headers)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_carries_no_headers() {
        let material = HeaderAuthMaterial::none();
        assert!(material.is_empty());
        assert!(material.headers().is_empty());
    }

    #[test]
    fn bearer_builds_authorization_header() {
        let material = HeaderAuthMaterial::bearer("tok_123").unwrap();
        assert_eq!(
            material.headers(),
            vec![("Authorization".to_string(), "Bearer tok_123".to_string())]
        );
    }

    #[test]
    fn custom_headers_preserve_order() {
        let material =
            HeaderAuthMaterial::from_headers([("X-First", "one"), ("X-Second", "two")]).unwrap();
        assert_eq!(
            material.headers(),
            vec![
                ("X-First".to_string(), "one".to_string()),
                ("X-Second".to_string(), "two".to_string())
            ]
        );
    }

    #[test]
    fn rejects_header_injection_shapes() {
        assert_eq!(
            HeaderAuthMaterial::from_headers([("", "x")]).unwrap_err(),
            AuthMaterialError::EmptyHeaderName
        );
        assert!(matches!(
            HeaderAuthMaterial::from_headers([("Bad Name", "x")]).unwrap_err(),
            AuthMaterialError::UnsafeHeaderName(_)
        ));
        assert!(matches!(
            HeaderAuthMaterial::from_headers([("X-Test", "x\r\nY: z")]).unwrap_err(),
            AuthMaterialError::UnsafeHeaderValue(_)
        ));
    }

    #[test]
    fn rejects_empty_or_control_bearer_tokens() {
        assert_eq!(
            HeaderAuthMaterial::bearer("  ").unwrap_err(),
            AuthMaterialError::EmptyBearerToken
        );
        assert_eq!(
            HeaderAuthMaterial::bearer("tok\nbad").unwrap_err(),
            AuthMaterialError::UnsafeBearerToken
        );
    }

    #[test]
    fn debug_redacts_header_values() {
        let debug = format!("{:?}", HeaderAuthMaterial::bearer("secret-token").unwrap());
        assert!(debug.contains("Authorization"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-token"));
    }
}
