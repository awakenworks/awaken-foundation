//! Grouped pagination.
//!
//! Compiles a filtered/sorted query that partitions rows by a grouping key,
//! producing the building blocks for a windowed query: the filter, the
//! partition column, and the per-group `ROW_NUMBER` / `COUNT` window
//! expressions. The consumer assembles the final `SELECT`, binds the
//! parameters, and packs results into `awaken_api_contract::GroupedPage`.
//!
//! Per-group cursoring is ordinary keyset pagination *within* a group: each
//! group's `cursor` (in the response envelope) is a [`crate::CursorState`]
//! over the same sort, scoped to that group's rows.

use awaken_api_contract::{ListQuery, QuerySchema};

use crate::build::{CompileOptions, FieldMap, FilterParam, QueryBuildError, compile};
use crate::dialect::Dialect;

/// A list query plus the field its rows are grouped by. The base query's sort
/// applies *within* each group, and its page size is the per-group limit.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupedQuery {
    pub base: ListQuery,
    pub group_by: String,
}

impl GroupedQuery {
    #[must_use]
    pub fn new(base: ListQuery, group_by: impl Into<String>) -> Self {
        Self {
            base,
            group_by: group_by.into(),
        }
    }
}

/// Compiled fragments for a grouped query. Window functions (`ROW_NUMBER`,
/// `COUNT`) are standard in PostgreSQL and SQLite ≥ 3.25, so these render the
/// same for both dialects.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupedSqlFragment {
    pub where_clause: Option<String>,
    pub group_column: String,
    /// `ORDER BY` body used to order rows within each partition (`None` when
    /// the base query has no sort).
    pub partition_order_by: Option<String>,
    pub per_group_limit: u32,
    pub params: Vec<FilterParam>,
}

impl GroupedSqlFragment {
    /// `ROW_NUMBER() OVER (PARTITION BY <group> ORDER BY <sort>) AS <alias>` —
    /// the per-group ordinal used to take the first N rows of each group.
    #[must_use]
    pub fn row_number_expr(&self, alias: &str) -> String {
        let mut window = format!("PARTITION BY {}", self.group_column);
        if let Some(order) = &self.partition_order_by {
            window.push_str(" ORDER BY ");
            window.push_str(order);
        }
        format!("ROW_NUMBER() OVER ({window}) AS {alias}")
    }

    /// `COUNT(*) OVER (PARTITION BY <group>) AS <alias>` — each group's full
    /// total, independent of the per-group page.
    #[must_use]
    pub fn group_total_expr(&self, alias: &str) -> String {
        format!(
            "COUNT(*) OVER (PARTITION BY {}) AS {alias}",
            self.group_column
        )
    }
}

/// Compile a grouped query. The grouping key must be allowlisted by `schema`
/// (`allow_group_field`) and mapped by `map`; otherwise this fails closed.
pub fn compile_grouped(
    query: &GroupedQuery,
    schema: &QuerySchema,
    map: &FieldMap,
    dialect: Dialect,
    opts: &CompileOptions,
) -> Result<GroupedSqlFragment, QueryBuildError> {
    if !schema.is_groupable(&query.group_by) {
        return Err(QueryBuildError::UngroupableField(query.group_by.clone()));
    }
    let group_column = map.resolve(&query.group_by)?.to_string();
    let base = compile(&query.base, map, dialect, opts)?;
    Ok(GroupedSqlFragment {
        where_clause: base.where_clause,
        group_column,
        partition_order_by: base.order_by,
        per_group_limit: base.limit,
        params: base.params,
    })
}
