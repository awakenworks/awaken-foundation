use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::page::{PaginationConfig, PaginationMode, PaginationRequest};

/// Parsed list request shared by read-model APIs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ListQuery {
    pub filter: FilterNode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortClause>,
    pub page: PaginationRequest,
}

/// Boolean operator for filter groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum BoolOperator {
    And,
    Or,
}

/// Filter AST node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FilterNode {
    Condition {
        field: String,
        operator: FilterOperator,
        value: FilterValue,
    },
    Group {
        operator: BoolOperator,
        conditions: Vec<FilterNode>,
    },
}

impl FilterNode {
    #[must_use]
    pub fn empty_and() -> Self {
        Self::Group {
            operator: BoolOperator::And,
            conditions: Vec::new(),
        }
    }

    #[must_use]
    pub fn and(conditions: Vec<FilterNode>) -> Self {
        Self::Group {
            operator: BoolOperator::And,
            conditions,
        }
    }
}

/// Supported comparison operator vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FilterOperator {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Like,
    Ilike,
    Contains,
    StartsWith,
    EndsWith,
    In,
    NotIn,
}

impl FromStr for FilterOperator {
    type Err = QueryError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "eq" => Ok(Self::Eq),
            "ne" => Ok(Self::Ne),
            "gt" => Ok(Self::Gt),
            "gte" => Ok(Self::Gte),
            "lt" => Ok(Self::Lt),
            "lte" => Ok(Self::Lte),
            "like" => Ok(Self::Like),
            "ilike" => Ok(Self::Ilike),
            "contains" => Ok(Self::Contains),
            "starts_with" | "startswith" | "startsWith" => Ok(Self::StartsWith),
            "ends_with" | "endswith" | "endsWith" => Ok(Self::EndsWith),
            "in" => Ok(Self::In),
            "not_in" | "notIn" | "nin" => Ok(Self::NotIn),
            other => Err(QueryError::UnsupportedFilterOperator(other.to_string())),
        }
    }
}

/// Filter value. URL query parsing preserves scalar strings unless a boolean,
/// null, or number is unambiguous.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FilterValue {
    Scalar(FilterScalar),
    List(Vec<FilterScalar>),
}

/// Scalar filter value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FilterScalar {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
}

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SortDirection {
    Asc,
    Desc,
}

/// One stable sort clause.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SortClause {
    pub field: String,
    pub direction: SortDirection,
}

impl SortClause {
    #[must_use]
    pub fn asc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            direction: SortDirection::Asc,
        }
    }

    #[must_use]
    pub fn desc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            direction: SortDirection::Desc,
        }
    }
}

/// Allowlist for one resource or read-model query surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuerySchema {
    filter_fields: BTreeMap<String, BTreeSet<FilterOperator>>,
    sortable_fields: BTreeSet<String>,
    default_sort: Vec<SortClause>,
    page: PaginationConfig,
}

impl QuerySchema {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_page_config(mut self, page: PaginationConfig) -> Self {
        self.page = page;
        self
    }

    #[must_use]
    pub fn allow_filter_field(
        mut self,
        field: impl Into<String>,
        operators: impl IntoIterator<Item = FilterOperator>,
    ) -> Self {
        self.filter_fields
            .insert(field.into(), operators.into_iter().collect());
        self
    }

    #[must_use]
    pub fn allow_sort_field(mut self, field: impl Into<String>) -> Self {
        self.sortable_fields.insert(field.into());
        self
    }

    #[must_use]
    pub fn with_default_sort(mut self, sort: Vec<SortClause>) -> Self {
        self.default_sort = sort;
        self
    }

    pub fn validate(&self, query: &ListQuery) -> Result<(), QueryError> {
        self.validate_filter(&query.filter)?;
        for clause in &query.sort {
            if !self.sortable_fields.contains(&clause.field) {
                return Err(QueryError::UnsupportedSortField(clause.field.clone()));
            }
        }
        Ok(())
    }

    fn validate_filter(&self, node: &FilterNode) -> Result<(), QueryError> {
        match node {
            FilterNode::Condition {
                field, operator, ..
            } => {
                let Some(operators) = self.filter_fields.get(field) else {
                    return Err(QueryError::UnsupportedFilterField(field.clone()));
                };
                if !operators.contains(operator) {
                    return Err(QueryError::OperatorNotAllowed {
                        field: field.clone(),
                        operator: *operator,
                    });
                }
                Ok(())
            }
            FilterNode::Group { conditions, .. } => {
                for condition in conditions {
                    self.validate_filter(condition)?;
                }
                Ok(())
            }
        }
    }
}

/// Query parse or validation failure.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum QueryError {
    #[error("invalid filter key '{0}'")]
    InvalidFilterKey(String),
    #[error("unsupported filter operator '{0}'")]
    UnsupportedFilterOperator(String),
    #[error("unsupported filter field '{0}'")]
    UnsupportedFilterField(String),
    #[error("operator '{operator:?}' is not allowed on filter field '{field}'")]
    OperatorNotAllowed {
        field: String,
        operator: FilterOperator,
    },
    #[error("unsupported sort field '{0}'")]
    UnsupportedSortField(String),
    #[error("invalid sort clause '{0}'")]
    InvalidSortClause(String),
    #[error("invalid page size '{0}'")]
    InvalidPageSize(String),
    #[error("invalid page number '{0}'")]
    InvalidPageNumber(String),
    #[error("invalid page offset '{0}'")]
    InvalidPageOffset(String),
    #[error("conflicting pagination parameters")]
    ConflictingPagination,
    #[error("page size {size} exceeds maximum {max}")]
    PageSizeTooLarge { size: u32, max: u32 },
}

/// Parse decoded query pairs into a validated [`ListQuery`].
///
/// Canonical keys are `filter[field]`, `filter[field][op]`, `sort`,
/// `page[size]`, and one of `page[cursor]`, `page[number]`, or `page[offset]`.
/// The parser also accepts `limit`, `cursor`, `page`, and `offset` as
/// compatibility aliases for existing APIs.
pub fn parse_list_query<I, K, V>(pairs: I, schema: &QuerySchema) -> Result<ListQuery, QueryError>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut filters = Vec::new();
    let mut sort = Vec::new();
    let mut page_size: Option<u32> = None;
    let mut cursor: Option<String> = None;
    let mut page_number: Option<u32> = None;
    let mut offset: Option<u64> = None;

    for (key, value) in pairs {
        let key = key.as_ref();
        let value = value.as_ref();
        if key == "sort" {
            sort.extend(parse_sort_value(value)?);
        } else if key == "page[size]" || key == "limit" {
            page_size = Some(parse_page_size(value, schema.page)?);
        } else if key == "page[cursor]" || key == "cursor" {
            if !value.trim().is_empty() {
                cursor = Some(value.to_string());
            }
        } else if key == "page[number]" || key == "page" {
            page_number = Some(parse_page_number(value)?);
        } else if key == "page[offset]" || key == "offset" {
            offset = Some(parse_page_offset(value)?);
        } else if key.starts_with("filter") {
            let (field, operator) = parse_filter_key(key)?;
            filters.push(FilterNode::Condition {
                field,
                operator,
                value: parse_filter_value(value, operator),
            });
        }
    }

    let sort = if sort.is_empty() {
        schema.default_sort.clone()
    } else {
        sort
    };
    let query = ListQuery {
        filter: FilterNode::and(filters),
        sort,
        page: build_pagination(
            page_size.unwrap_or(schema.page.default_size),
            cursor,
            page_number,
            offset,
            schema.page.default_mode,
        )?,
    };
    schema.validate(&query)?;
    Ok(query)
}

fn parse_filter_key(key: &str) -> Result<(String, FilterOperator), QueryError> {
    let parts = bracket_parts(key).ok_or_else(|| QueryError::InvalidFilterKey(key.to_string()))?;
    match parts.as_slice() {
        [field] if !field.is_empty() => Ok(((*field).to_string(), FilterOperator::Eq)),
        [field, operator] if !field.is_empty() && !operator.is_empty() => {
            Ok(((*field).to_string(), FilterOperator::from_str(operator)?))
        }
        _ => Err(QueryError::InvalidFilterKey(key.to_string())),
    }
}

fn bracket_parts(key: &str) -> Option<Vec<&str>> {
    let rest = key.strip_prefix("filter")?;
    if rest.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut remaining = rest;
    while let Some(after_open) = remaining.strip_prefix('[') {
        let (part, after_part) = after_open.split_once(']')?;
        parts.push(part);
        remaining = after_part;
    }
    if remaining.is_empty() {
        Some(parts)
    } else {
        None
    }
}

fn parse_filter_value(value: &str, operator: FilterOperator) -> FilterValue {
    if matches!(operator, FilterOperator::In | FilterOperator::NotIn) {
        return FilterValue::List(value.split(',').map(parse_scalar).collect());
    }
    FilterValue::Scalar(parse_scalar(value))
}

fn parse_scalar(value: &str) -> FilterScalar {
    match value {
        "true" => FilterScalar::Bool(true),
        "false" => FilterScalar::Bool(false),
        "null" => FilterScalar::Null,
        _ => value.parse::<f64>().map_or_else(
            |_| FilterScalar::String(value.to_string()),
            FilterScalar::Number,
        ),
    }
}

fn parse_sort_value(raw: &str) -> Result<Vec<SortClause>, QueryError> {
    let mut clauses = Vec::new();
    for part in raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let (direction, field) = if let Some(field) = part.strip_prefix('-') {
            (SortDirection::Desc, field)
        } else {
            (SortDirection::Asc, part)
        };
        if field.is_empty() {
            return Err(QueryError::InvalidSortClause(part.to_string()));
        }
        clauses.push(SortClause {
            field: field.to_string(),
            direction,
        });
    }
    Ok(clauses)
}

fn parse_page_size(raw: &str, config: PaginationConfig) -> Result<u32, QueryError> {
    let size = raw
        .parse::<u32>()
        .map_err(|_| QueryError::InvalidPageSize(raw.to_string()))?;
    if size == 0 {
        return Err(QueryError::InvalidPageSize(raw.to_string()));
    }
    if size > config.max_size {
        return Err(QueryError::PageSizeTooLarge {
            size,
            max: config.max_size,
        });
    }
    Ok(size)
}

fn parse_page_number(raw: &str) -> Result<u32, QueryError> {
    let number = raw
        .parse::<u32>()
        .map_err(|_| QueryError::InvalidPageNumber(raw.to_string()))?;
    if number == 0 {
        return Err(QueryError::InvalidPageNumber(raw.to_string()));
    }
    Ok(number)
}

fn parse_page_offset(raw: &str) -> Result<u64, QueryError> {
    raw.parse::<u64>()
        .map_err(|_| QueryError::InvalidPageOffset(raw.to_string()))
}

fn build_pagination(
    size: u32,
    cursor: Option<String>,
    page_number: Option<u32>,
    offset: Option<u64>,
    default_mode: PaginationMode,
) -> Result<PaginationRequest, QueryError> {
    let mode_count = usize::from(cursor.is_some())
        + usize::from(page_number.is_some())
        + usize::from(offset.is_some());
    if mode_count > 1 {
        return Err(QueryError::ConflictingPagination);
    }

    if let Some(cursor) = cursor {
        return Ok(PaginationRequest::cursor(size, Some(cursor)));
    }
    if let Some(number) = page_number {
        return Ok(PaginationRequest::page(size, number));
    }
    if let Some(offset) = offset {
        return Ok(PaginationRequest::offset(size, offset));
    }

    Ok(match default_mode {
        PaginationMode::Cursor => PaginationRequest::cursor(size, None),
        PaginationMode::Page => PaginationRequest::page(size, 1),
        PaginationMode::Offset => PaginationRequest::offset(size, 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_completion_schema() -> QuerySchema {
        QuerySchema::new()
            .allow_filter_field("project_id", [FilterOperator::Eq, FilterOperator::In])
            .allow_filter_field(
                "completed_at",
                [
                    FilterOperator::Gt,
                    FilterOperator::Gte,
                    FilterOperator::Lt,
                    FilterOperator::Lte,
                ],
            )
            .allow_filter_field("category", [FilterOperator::Eq, FilterOperator::In])
            .allow_sort_field("completed_at")
            .allow_sort_field("issue_id")
            .with_default_sort(vec![
                SortClause::desc("completed_at"),
                SortClause::asc("issue_id"),
            ])
            .with_page_config(PaginationConfig::new(20, 100))
    }

    #[test]
    fn parses_bracket_filters_sort_and_cursor_page() {
        let query = parse_list_query(
            [
                ("filter[project_id]", "019ede"),
                ("filter[completed_at][gt]", "2026-06-26T18:12:25Z"),
                ("filter[category][in]", "done,accepted"),
                ("sort", "-completed_at,issue_id"),
                ("page[size]", "50"),
                ("page[cursor]", "opaque"),
            ],
            &issue_completion_schema(),
        )
        .unwrap();

        assert_eq!(query.page.size(), 50);
        assert_eq!(query.page.cursor_value(), Some("opaque"));
        assert_eq!(
            query.sort,
            vec![
                SortClause::desc("completed_at"),
                SortClause::asc("issue_id")
            ]
        );
        let FilterNode::Group { conditions, .. } = query.filter else {
            panic!("expected group");
        };
        assert_eq!(conditions.len(), 3);
    }

    #[test]
    fn applies_default_sort() {
        let query = parse_list_query(
            std::iter::empty::<(&str, &str)>(),
            &issue_completion_schema(),
        )
        .unwrap();
        assert_eq!(
            query.sort,
            vec![
                SortClause::desc("completed_at"),
                SortClause::asc("issue_id")
            ]
        );
        assert_eq!(query.page.size(), 20);
    }

    #[test]
    fn rejects_unknown_filter_field() {
        let err = parse_list_query([("filter[state_key]", "done")], &issue_completion_schema())
            .unwrap_err();
        assert_eq!(
            err,
            QueryError::UnsupportedFilterField("state_key".to_string())
        );
    }

    #[test]
    fn rejects_disallowed_operator() {
        let err = parse_list_query(
            [("filter[category][gt]", "done")],
            &issue_completion_schema(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            QueryError::OperatorNotAllowed {
                field: "category".to_string(),
                operator: FilterOperator::Gt
            }
        );
    }

    #[test]
    fn rejects_unknown_sort_field() {
        let err =
            parse_list_query([("sort", "-state_key")], &issue_completion_schema()).unwrap_err();
        assert_eq!(
            err,
            QueryError::UnsupportedSortField("state_key".to_string())
        );
    }

    #[test]
    fn rejects_page_size_above_surface_limit() {
        let err =
            parse_list_query([("page[size]", "101")], &issue_completion_schema()).unwrap_err();
        assert_eq!(
            err,
            QueryError::PageSizeTooLarge {
                size: 101,
                max: 100
            }
        );
    }

    #[test]
    fn parses_every_bracket_operator_alias() {
        let schema = QuerySchema::new()
            .allow_filter_field(
                "field",
                [
                    FilterOperator::Eq,
                    FilterOperator::Ne,
                    FilterOperator::Gt,
                    FilterOperator::Gte,
                    FilterOperator::Lt,
                    FilterOperator::Lte,
                    FilterOperator::Like,
                    FilterOperator::Ilike,
                    FilterOperator::Contains,
                    FilterOperator::StartsWith,
                    FilterOperator::EndsWith,
                    FilterOperator::In,
                    FilterOperator::NotIn,
                ],
            )
            .with_page_config(PaginationConfig::new(20, 100));
        let cases = [
            ("filter[field]", FilterOperator::Eq),
            ("filter[field][eq]", FilterOperator::Eq),
            ("filter[field][ne]", FilterOperator::Ne),
            ("filter[field][gt]", FilterOperator::Gt),
            ("filter[field][gte]", FilterOperator::Gte),
            ("filter[field][lt]", FilterOperator::Lt),
            ("filter[field][lte]", FilterOperator::Lte),
            ("filter[field][like]", FilterOperator::Like),
            ("filter[field][ilike]", FilterOperator::Ilike),
            ("filter[field][contains]", FilterOperator::Contains),
            ("filter[field][startswith]", FilterOperator::StartsWith),
            ("filter[field][startsWith]", FilterOperator::StartsWith),
            ("filter[field][endswith]", FilterOperator::EndsWith),
            ("filter[field][endsWith]", FilterOperator::EndsWith),
            ("filter[field][in]", FilterOperator::In),
            ("filter[field][nin]", FilterOperator::NotIn),
            ("filter[field][notIn]", FilterOperator::NotIn),
            ("filter[field][not_in]", FilterOperator::NotIn),
        ];

        for (key, expected) in cases {
            let query = parse_list_query([(key, "value")], &schema).unwrap();
            let FilterNode::Group { conditions, .. } = query.filter else {
                panic!("expected group");
            };
            let FilterNode::Condition { operator, .. } = &conditions[0] else {
                panic!("expected condition");
            };
            assert_eq!(*operator, expected, "operator for {key}");
        }
    }

    #[test]
    fn parses_filter_scalars_and_lists() {
        let schema = QuerySchema::new()
            .allow_filter_field("flag", [FilterOperator::Eq])
            .allow_filter_field("count", [FilterOperator::Eq])
            .allow_filter_field("empty", [FilterOperator::Eq])
            .allow_filter_field("tags", [FilterOperator::In])
            .with_page_config(PaginationConfig::new(20, 100));

        let query = parse_list_query(
            [
                ("filter[flag]", "true"),
                ("filter[count]", "42"),
                ("filter[empty]", "null"),
                ("filter[tags][in]", "alpha,2,false,null"),
            ],
            &schema,
        )
        .unwrap();

        let FilterNode::Group { conditions, .. } = query.filter else {
            panic!("expected group");
        };
        assert_eq!(conditions.len(), 4);
        assert_eq!(
            conditions[0],
            FilterNode::Condition {
                field: "flag".to_string(),
                operator: FilterOperator::Eq,
                value: FilterValue::Scalar(FilterScalar::Bool(true))
            }
        );
        assert_eq!(
            conditions[1],
            FilterNode::Condition {
                field: "count".to_string(),
                operator: FilterOperator::Eq,
                value: FilterValue::Scalar(FilterScalar::Number(42.0))
            }
        );
        assert_eq!(
            conditions[2],
            FilterNode::Condition {
                field: "empty".to_string(),
                operator: FilterOperator::Eq,
                value: FilterValue::Scalar(FilterScalar::Null)
            }
        );
        assert_eq!(
            conditions[3],
            FilterNode::Condition {
                field: "tags".to_string(),
                operator: FilterOperator::In,
                value: FilterValue::List(vec![
                    FilterScalar::String("alpha".to_string()),
                    FilterScalar::Number(2.0),
                    FilterScalar::Bool(false),
                    FilterScalar::Null,
                ])
            }
        );
    }

    #[test]
    fn rejects_malformed_filter_keys_and_unknown_operators() {
        let schema = QuerySchema::new()
            .allow_filter_field("field", [FilterOperator::Eq])
            .with_page_config(PaginationConfig::new(20, 100));

        assert_eq!(
            parse_list_query([("filter", "x")], &schema).unwrap_err(),
            QueryError::InvalidFilterKey("filter".to_string())
        );
        assert_eq!(
            parse_list_query([("filter[][eq]", "x")], &schema).unwrap_err(),
            QueryError::InvalidFilterKey("filter[][eq]".to_string())
        );
        assert_eq!(
            parse_list_query([("filter[field][between]", "x")], &schema).unwrap_err(),
            QueryError::UnsupportedFilterOperator("between".to_string())
        );
    }

    #[test]
    fn parses_page_number_pagination() {
        let query = parse_list_query(
            [("page[size]", "10"), ("page[number]", "3")],
            &issue_completion_schema(),
        )
        .unwrap();
        assert_eq!(query.page, PaginationRequest::page(10, 3));
    }

    #[test]
    fn parses_offset_pagination() {
        let query = parse_list_query(
            [("limit", "10"), ("offset", "40")],
            &issue_completion_schema(),
        )
        .unwrap();
        assert_eq!(query.page, PaginationRequest::offset(10, 40));
    }

    #[test]
    fn rejects_mixed_pagination_modes() {
        let err = parse_list_query(
            [("cursor", "abc"), ("page[number]", "2")],
            &issue_completion_schema(),
        )
        .unwrap_err();
        assert_eq!(err, QueryError::ConflictingPagination);
    }

    #[test]
    fn honors_non_cursor_default_pagination_modes() {
        let page_schema = issue_completion_schema().with_page_config(
            PaginationConfig::new(25, 100).with_default_mode(PaginationMode::Page),
        );
        let offset_schema = issue_completion_schema().with_page_config(
            PaginationConfig::new(25, 100).with_default_mode(PaginationMode::Offset),
        );

        assert_eq!(
            parse_list_query(std::iter::empty::<(&str, &str)>(), &page_schema)
                .unwrap()
                .page,
            PaginationRequest::page(25, 1)
        );
        assert_eq!(
            parse_list_query(std::iter::empty::<(&str, &str)>(), &offset_schema)
                .unwrap()
                .page,
            PaginationRequest::offset(25, 0)
        );
    }

    #[test]
    fn rejects_invalid_numbered_pagination_values() {
        assert_eq!(
            parse_list_query([("page[number]", "0")], &issue_completion_schema()).unwrap_err(),
            QueryError::InvalidPageNumber("0".to_string())
        );
        assert_eq!(
            parse_list_query([("page[number]", "-1")], &issue_completion_schema()).unwrap_err(),
            QueryError::InvalidPageNumber("-1".to_string())
        );
        assert_eq!(
            parse_list_query([("offset", "-1")], &issue_completion_schema()).unwrap_err(),
            QueryError::InvalidPageOffset("-1".to_string())
        );
    }
}
