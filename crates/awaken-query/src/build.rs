//! Compile a validated [`ListQuery`] into a parameterized SQL fragment.
//!
//! The query is assumed already validated by `awaken-api-contract`'s
//! `QuerySchema` (field/operator allowlist). This module never emits a field
//! name from the request: every column comes from the author-controlled
//! [`FieldMap`], and every value is a bound parameter. The result is a fragment
//! plus ordered parameters — it is never executed here.

use std::collections::BTreeMap;

use awaken_api_contract::{
    BoolOperator, FilterNode, FilterOperator, FilterScalar, FilterValue, ListQuery, QuerySchema,
    SortClause, SortDirection,
};

use crate::dialect::{Dialect, ParamBuilder};

/// A value bound to a placeholder. The consumer maps each variant onto its
/// driver's parameter type when executing.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterParam {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
    /// A whole set bound as one parameter (Postgres `= ANY($1)`).
    Array(Vec<FilterParam>),
}

impl From<&FilterScalar> for FilterParam {
    fn from(scalar: &FilterScalar) -> Self {
        match scalar {
            FilterScalar::String(s) => Self::String(s.clone()),
            FilterScalar::Number(n) => Self::Number(*n),
            FilterScalar::Bool(b) => Self::Bool(*b),
            FilterScalar::Null => Self::Null,
        }
    }
}

/// Logical field → physical column (or SQL expression) mapping.
///
/// The column side is **author-controlled** and emitted verbatim, so it may be
/// a qualified name (`t.created_at`) or an expression. It must never be derived
/// from request input. A field absent from the map is rejected rather than
/// passed through, so a request can never name a storage column directly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldMap {
    columns: BTreeMap<String, String>,
}

impl FieldMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Map a logical field to a physical column or SQL expression.
    #[must_use]
    pub fn column(mut self, field: impl Into<String>, column: impl Into<String>) -> Self {
        self.columns.insert(field.into(), column.into());
        self
    }

    /// Identity map over every field the schema allowlists (filter, sort, and
    /// group fields map to a column of the same name). Override specific
    /// columns afterwards with [`FieldMap::column`].
    #[must_use]
    pub fn from_schema(schema: &QuerySchema) -> Self {
        let mut columns = BTreeMap::new();
        for name in schema
            .filter_field_names()
            .chain(schema.sortable_field_names())
            .chain(schema.groupable_field_names())
        {
            columns.insert(name.to_string(), name.to_string());
        }
        Self { columns }
    }

    pub(crate) fn resolve(&self, field: &str) -> Result<&str, QueryBuildError> {
        self.columns
            .get(field)
            .map(String::as_str)
            .ok_or_else(|| QueryBuildError::UnmappedField(field.to_string()))
    }
}

/// Build-time bounds and parameter numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileOptions {
    /// Maximum element count in an `in` / `not_in` set.
    pub max_array_size: usize,
    /// Maximum length of a string value.
    pub max_string_len: usize,
    /// 1-based index of the first bound parameter, so callers can compose this
    /// fragment after their own leading parameters (e.g. a tenancy predicate).
    pub start_param: usize,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            max_array_size: 100,
            max_string_len: 1000,
            start_param: 1,
        }
    }
}

/// Compiled SQL fragment. `where_clause` and `order_by` are `None` when the
/// query carries no filter / no sort, so the consumer can omit the clause.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlFragment {
    pub where_clause: Option<String>,
    pub order_by: Option<String>,
    pub limit: u32,
    pub params: Vec<FilterParam>,
}

impl SqlFragment {
    /// Number of bound parameters in this fragment.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.params.len()
    }
}

/// Filter/sort compilation failure.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum QueryBuildError {
    #[error("field '{0}' is not mapped to a column")]
    UnmappedField(String),
    #[error("operator '{operator:?}' on field '{field}' requires a {expected} value")]
    ValueShapeMismatch {
        field: String,
        operator: FilterOperator,
        expected: ValueShape,
    },
    #[error("set on field '{field}' has {size} elements, exceeding the maximum {max}")]
    ArrayTooLarge {
        field: String,
        size: usize,
        max: usize,
    },
    #[error("string on field '{field}' is {len} chars, exceeding the maximum {max}")]
    StringTooLong {
        field: String,
        len: usize,
        max: usize,
    },
    #[error("invalid cursor: {0}")]
    InvalidCursor(String),
    #[error("field '{0}' is not allowed as a grouping key")]
    UngroupableField(String),
}

/// The value shape an operator expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueShape {
    Scalar,
    StringScalar,
    Set,
}

impl std::fmt::Display for ValueShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Scalar => "scalar",
            Self::StringScalar => "string",
            Self::Set => "list",
        };
        f.write_str(label)
    }
}

/// Compile a validated list query into a SQL fragment for `dialect`.
pub fn compile(
    query: &ListQuery,
    map: &FieldMap,
    dialect: Dialect,
    opts: &CompileOptions,
) -> Result<SqlFragment, QueryBuildError> {
    let mut params = dialect.params(opts.start_param);
    let where_clause = render_node(&query.filter, map, dialect, opts, &mut params)?;
    let order_by = render_order_by(&query.sort, map)?;
    Ok(SqlFragment {
        where_clause,
        order_by,
        limit: query.page.size(),
        params: params.into_params(),
    })
}

/// Render a sort spec into an `ORDER BY` body (without the keyword). `None`
/// when there are no clauses.
pub(crate) fn render_order_by(
    sort: &[SortClause],
    map: &FieldMap,
) -> Result<Option<String>, QueryBuildError> {
    if sort.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::with_capacity(sort.len());
    for clause in sort {
        let column = map.resolve(&clause.field)?;
        let direction = match clause.direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        parts.push(format!("{column} {direction}"));
    }
    Ok(Some(parts.join(", ")))
}

fn render_node(
    node: &FilterNode,
    map: &FieldMap,
    dialect: Dialect,
    opts: &CompileOptions,
    params: &mut ParamBuilder,
) -> Result<Option<String>, QueryBuildError> {
    match node {
        FilterNode::Condition {
            field,
            operator,
            value,
        } => render_condition(field, *operator, value, map, dialect, opts, params).map(Some),
        FilterNode::Group {
            operator,
            conditions,
        } => {
            let joiner = match operator {
                BoolOperator::And => " AND ",
                BoolOperator::Or => " OR ",
            };
            let mut parts = Vec::with_capacity(conditions.len());
            for child in conditions {
                if let Some(rendered) = render_node(child, map, dialect, opts, params)? {
                    // Parenthesize nested groups so precedence is explicit.
                    if matches!(child, FilterNode::Group { .. }) {
                        parts.push(format!("({rendered})"));
                    } else {
                        parts.push(rendered);
                    }
                }
            }
            if parts.is_empty() {
                Ok(None)
            } else {
                Ok(Some(parts.join(joiner)))
            }
        }
    }
}

fn render_condition(
    field: &str,
    operator: FilterOperator,
    value: &FilterValue,
    map: &FieldMap,
    dialect: Dialect,
    opts: &CompileOptions,
    params: &mut ParamBuilder,
) -> Result<String, QueryBuildError> {
    let column = map.resolve(field)?;
    match operator {
        FilterOperator::In | FilterOperator::NotIn => {
            let scalars = expect_set(field, operator, value)?;
            check_set_bounds(field, scalars, opts)?;
            Ok(render_set(
                column,
                scalars,
                matches!(operator, FilterOperator::NotIn),
                dialect,
                params,
            ))
        }
        FilterOperator::Like | FilterOperator::Ilike => {
            let text = expect_string(field, operator, value, opts)?;
            let keyword = match operator {
                FilterOperator::Like => "LIKE",
                _ => dialect.ilike_keyword(),
            };
            let placeholder = params.bind(FilterParam::String(text.to_string()));
            Ok(format!("{column} {keyword} {placeholder}"))
        }
        FilterOperator::Contains | FilterOperator::StartsWith | FilterOperator::EndsWith => {
            let text = expect_string(field, operator, value, opts)?;
            let pattern = like_pattern(operator, text);
            let placeholder = params.bind(FilterParam::String(pattern));
            // Case-insensitive substring/prefix/suffix match. ESCAPE makes the
            // wrapped value's `% _ \` literal rather than wildcard.
            Ok(format!(
                "{column} {keyword} {placeholder} ESCAPE '\\'",
                keyword = dialect.ilike_keyword()
            ))
        }
        FilterOperator::Eq | FilterOperator::Ne => {
            let scalar = expect_scalar(field, operator, value)?;
            if matches!(scalar, FilterScalar::Null) {
                let sql = if matches!(operator, FilterOperator::Eq) {
                    "IS NULL"
                } else {
                    "IS NOT NULL"
                };
                return Ok(format!("{column} {sql}"));
            }
            check_scalar_bounds(field, scalar, opts)?;
            let op = if matches!(operator, FilterOperator::Eq) {
                "="
            } else {
                "<>"
            };
            let placeholder = params.bind(scalar.into());
            Ok(format!("{column} {op} {placeholder}"))
        }
        FilterOperator::Gt | FilterOperator::Gte | FilterOperator::Lt | FilterOperator::Lte => {
            let scalar = expect_scalar(field, operator, value)?;
            check_scalar_bounds(field, scalar, opts)?;
            let op = match operator {
                FilterOperator::Gt => ">",
                FilterOperator::Gte => ">=",
                FilterOperator::Lt => "<",
                _ => "<=",
            };
            let placeholder = params.bind(scalar.into());
            Ok(format!("{column} {op} {placeholder}"))
        }
    }
}

fn render_set(
    column: &str,
    scalars: &[FilterScalar],
    negated: bool,
    dialect: Dialect,
    params: &mut ParamBuilder,
) -> String {
    if dialect.binds_set_as_array() {
        let placeholder = params.bind(FilterParam::Array(scalars.iter().map(Into::into).collect()));
        if negated {
            format!("{column} != ALL({placeholder})")
        } else {
            format!("{column} = ANY({placeholder})")
        }
    } else if scalars.is_empty() {
        // SQLite has no `IN ()`. Empty `in` matches nothing; empty `not_in`
        // matches everything.
        if negated { "1 = 1" } else { "1 = 0" }.to_string()
    } else {
        let placeholders: Vec<String> = scalars
            .iter()
            .map(|scalar| params.bind(scalar.into()))
            .collect();
        let keyword = if negated { "NOT IN" } else { "IN" };
        format!("{column} {keyword} ({})", placeholders.join(", "))
    }
}

fn expect_set<'a>(
    field: &str,
    operator: FilterOperator,
    value: &'a FilterValue,
) -> Result<&'a [FilterScalar], QueryBuildError> {
    match value {
        FilterValue::List(items) => Ok(items),
        FilterValue::Scalar(_) => Err(QueryBuildError::ValueShapeMismatch {
            field: field.to_string(),
            operator,
            expected: ValueShape::Set,
        }),
    }
}

fn expect_scalar<'a>(
    field: &str,
    operator: FilterOperator,
    value: &'a FilterValue,
) -> Result<&'a FilterScalar, QueryBuildError> {
    match value {
        FilterValue::Scalar(scalar) => Ok(scalar),
        FilterValue::List(_) => Err(QueryBuildError::ValueShapeMismatch {
            field: field.to_string(),
            operator,
            expected: ValueShape::Scalar,
        }),
    }
}

fn expect_string<'a>(
    field: &str,
    operator: FilterOperator,
    value: &'a FilterValue,
    opts: &CompileOptions,
) -> Result<&'a str, QueryBuildError> {
    match value {
        FilterValue::Scalar(FilterScalar::String(text)) => {
            check_string_len(field, text, opts)?;
            Ok(text)
        }
        _ => Err(QueryBuildError::ValueShapeMismatch {
            field: field.to_string(),
            operator,
            expected: ValueShape::StringScalar,
        }),
    }
}

fn like_pattern(operator: FilterOperator, text: &str) -> String {
    let escaped = escape_like(text);
    match operator {
        FilterOperator::Contains => format!("%{escaped}%"),
        FilterOperator::StartsWith => format!("{escaped}%"),
        _ => format!("%{escaped}"),
    }
}

fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn check_set_bounds(
    field: &str,
    scalars: &[FilterScalar],
    opts: &CompileOptions,
) -> Result<(), QueryBuildError> {
    if scalars.len() > opts.max_array_size {
        return Err(QueryBuildError::ArrayTooLarge {
            field: field.to_string(),
            size: scalars.len(),
            max: opts.max_array_size,
        });
    }
    for scalar in scalars {
        check_scalar_bounds(field, scalar, opts)?;
    }
    Ok(())
}

fn check_scalar_bounds(
    field: &str,
    scalar: &FilterScalar,
    opts: &CompileOptions,
) -> Result<(), QueryBuildError> {
    if let FilterScalar::String(text) = scalar {
        check_string_len(field, text, opts)?;
    }
    Ok(())
}

fn check_string_len(field: &str, text: &str, opts: &CompileOptions) -> Result<(), QueryBuildError> {
    if text.chars().count() > opts.max_string_len {
        return Err(QueryBuildError::StringTooLong {
            field: field.to_string(),
            len: text.chars().count(),
            max: opts.max_string_len,
        });
    }
    Ok(())
}
