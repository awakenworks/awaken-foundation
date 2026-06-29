//! Opaque keyset cursors.
//!
//! A cursor carries the sort-key values of the last row of a page. From a sort
//! spec plus that state, [`keyset_condition`] builds the compound row-value
//! `WHERE` that resumes after the cursor. The token itself is opaque
//! (base64-encoded JSON) and shares the contract crate's opaque-cursor
//! convention.

use std::collections::BTreeMap;

use awaken_api_contract::{SortClause, SortDirection};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::build::{FieldMap, FilterParam, QueryBuildError};
use crate::dialect::Dialect;

/// The sort-key values identifying a page boundary. Keys are logical field
/// names (matching the sort clauses); values are scalar JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CursorState {
    values: BTreeMap<String, serde_json::Value>,
}

impl CursorState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the boundary value for one sort field.
    #[must_use]
    pub fn set(mut self, field: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.values.insert(field.into(), value.into());
        self
    }

    /// Encode to an opaque token.
    #[must_use]
    pub fn encode(&self) -> String {
        // Serializing a String-keyed map of JSON values never fails.
        let json = serde_json::to_vec(&self.values).unwrap_or_else(|_| b"{}".to_vec());
        URL_SAFE_NO_PAD.encode(json)
    }

    /// Decode an opaque token produced by [`CursorState::encode`].
    pub fn decode(token: &str) -> Result<Self, QueryBuildError> {
        if token.is_empty() {
            return Err(QueryBuildError::InvalidCursor("empty cursor".to_string()));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| QueryBuildError::InvalidCursor("malformed base64".to_string()))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| QueryBuildError::InvalidCursor("malformed json".to_string()))?;
        match value {
            serde_json::Value::Object(map) => Ok(Self {
                values: map.into_iter().collect(),
            }),
            _ => Err(QueryBuildError::InvalidCursor(
                "cursor must be an object".to_string(),
            )),
        }
    }

    fn lookup(&self, field: &str) -> Result<&serde_json::Value, QueryBuildError> {
        self.values.get(field).ok_or_else(|| {
            QueryBuildError::InvalidCursor(format!("cursor is missing sort field '{field}'"))
        })
    }
}

/// Build the keyset `WHERE` body that resumes strictly after (`backward =
/// false`) or before (`backward = true`) the cursor, for the given sort.
///
/// The sort must be a total order (its trailing clause a unique key) for
/// pagination to be gap-free; this is the caller's responsibility. Returns the
/// clause body (no `WHERE` keyword) and the parameters in placeholder order.
pub fn keyset_condition(
    sort: &[SortClause],
    cursor: &CursorState,
    map: &FieldMap,
    dialect: Dialect,
    start_param: usize,
    backward: bool,
) -> Result<(String, Vec<FilterParam>), QueryBuildError> {
    if sort.is_empty() {
        return Err(QueryBuildError::InvalidCursor(
            "keyset pagination requires at least one sort clause".to_string(),
        ));
    }

    let mut params = dialect.params(start_param);
    let mut disjuncts = Vec::with_capacity(sort.len());

    for i in 0..sort.len() {
        let current = &sort[i];
        let column = map.resolve(&current.field)?;
        let op = comparison_op(current.direction, backward);
        let value = cursor.lookup(&current.field)?;

        if i == 0 {
            let placeholder = params.bind(json_to_param(value)?);
            disjuncts.push(format!("{column} {op} {placeholder}"));
        } else {
            let mut conjuncts = Vec::with_capacity(i + 1);
            for prefix in &sort[..i] {
                let prefix_column = map.resolve(&prefix.field)?;
                let prefix_value = cursor.lookup(&prefix.field)?;
                let placeholder = params.bind(json_to_param(prefix_value)?);
                conjuncts.push(format!("{prefix_column} = {placeholder}"));
            }
            let placeholder = params.bind(json_to_param(value)?);
            conjuncts.push(format!("{column} {op} {placeholder}"));
            disjuncts.push(format!("({})", conjuncts.join(" AND ")));
        }
    }

    let clause = format!("({})", disjuncts.join(" OR "));
    Ok((clause, params.into_params()))
}

const fn comparison_op(direction: SortDirection, backward: bool) -> &'static str {
    let ascending = matches!(direction, SortDirection::Asc);
    // Forward + ascending walks upward (`>`); any single flip reverses it.
    if ascending == backward { "<" } else { ">" }
}

fn json_to_param(value: &serde_json::Value) -> Result<FilterParam, QueryBuildError> {
    match value {
        serde_json::Value::Null => Ok(FilterParam::Null),
        serde_json::Value::Bool(b) => Ok(FilterParam::Bool(*b)),
        serde_json::Value::Number(n) => n.as_f64().map(FilterParam::Number).ok_or_else(|| {
            QueryBuildError::InvalidCursor("cursor number out of range".to_string())
        }),
        serde_json::Value::String(s) => Ok(FilterParam::String(s.clone())),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Err(
            QueryBuildError::InvalidCursor("cursor values must be scalar".to_string()),
        ),
    }
}
