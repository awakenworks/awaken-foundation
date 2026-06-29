//! Grouped-pagination compilation.

use awaken_api_contract::{
    FilterOperator, PaginationConfig, QuerySchema, SortClause, parse_list_query,
};
use awaken_query::{
    CompileOptions, Dialect, FieldMap, GroupedQuery, QueryBuildError, compile_grouped,
};

fn schema() -> QuerySchema {
    QuerySchema::new()
        .allow_filter_field("project_id", [FilterOperator::Eq])
        .allow_sort_field("completed_at")
        .allow_sort_field("issue_id")
        .allow_group_field("category")
        .with_default_sort(vec![
            SortClause::desc("completed_at"),
            SortClause::asc("issue_id"),
        ])
        .with_page_config(PaginationConfig::new(10, 100))
}

fn field_map() -> FieldMap {
    FieldMap::from_schema(&schema())
        .column("project_id", "project_id")
        .column("category", "category_key")
}

fn grouped(group_by: &str) -> GroupedQuery {
    let base = parse_list_query([("filter[project_id]", "p-1")], &schema()).unwrap();
    GroupedQuery::new(base, group_by)
}

#[test]
fn compiles_partition_and_window_expressions() {
    let fragment = compile_grouped(
        &grouped("category"),
        &schema(),
        &field_map(),
        Dialect::Postgres,
        &CompileOptions::default(),
    )
    .unwrap();

    assert_eq!(fragment.where_clause.as_deref(), Some("project_id = $1"));
    assert_eq!(fragment.group_column, "category_key");
    assert_eq!(
        fragment.partition_order_by.as_deref(),
        Some("completed_at DESC, issue_id ASC")
    );
    assert_eq!(fragment.per_group_limit, 10);

    assert_eq!(
        fragment.row_number_expr("rn"),
        "ROW_NUMBER() OVER (PARTITION BY category_key ORDER BY completed_at DESC, issue_id ASC) AS rn"
    );
    assert_eq!(
        fragment.group_total_expr("group_total"),
        "COUNT(*) OVER (PARTITION BY category_key) AS group_total"
    );
}

#[test]
fn window_omits_order_by_when_unsorted() {
    let mut base = parse_list_query([("filter[project_id]", "p-1")], &schema()).unwrap();
    base.sort.clear();
    let fragment = compile_grouped(
        &GroupedQuery::new(base, "category"),
        &schema(),
        &field_map(),
        Dialect::Postgres,
        &CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(fragment.partition_order_by, None);
    assert_eq!(
        fragment.row_number_expr("rn"),
        "ROW_NUMBER() OVER (PARTITION BY category_key) AS rn"
    );
}

#[test]
fn non_groupable_field_fails_closed() {
    // `project_id` is filterable but never opted into grouping.
    let err = compile_grouped(
        &grouped("project_id"),
        &schema(),
        &field_map(),
        Dialect::Postgres,
        &CompileOptions::default(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        QueryBuildError::UngroupableField("project_id".to_string())
    );
}

#[test]
fn unmapped_group_field_is_rejected() {
    let schema = schema().allow_group_field("status");
    let err = compile_grouped(
        &grouped("status"),
        &schema,
        &field_map(),
        Dialect::Postgres,
        &CompileOptions::default(),
    )
    .unwrap_err();
    assert_eq!(err, QueryBuildError::UnmappedField("status".to_string()));
}
