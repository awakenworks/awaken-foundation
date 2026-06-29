//! Filter / sort compilation across dialects.

use awaken_api_contract::{
    BoolOperator, FilterNode, FilterOperator, FilterScalar, FilterValue, ListQuery,
    PaginationConfig, PaginationRequest, QuerySchema, SortClause, parse_list_query,
};
use awaken_query::{CompileOptions, Dialect, FieldMap, FilterParam, QueryBuildError, compile};

fn schema() -> QuerySchema {
    QuerySchema::new()
        .allow_filter_field(
            "project_id",
            [
                FilterOperator::Eq,
                FilterOperator::In,
                FilterOperator::NotIn,
            ],
        )
        .allow_filter_field(
            "completed_at",
            [
                FilterOperator::Gt,
                FilterOperator::Gte,
                FilterOperator::Lt,
                FilterOperator::Lte,
            ],
        )
        .allow_filter_field("title", [FilterOperator::Contains, FilterOperator::Like])
        .allow_filter_field("name", [FilterOperator::Ilike, FilterOperator::StartsWith])
        .allow_filter_field("archived_at", [FilterOperator::Eq, FilterOperator::Ne])
        .allow_filter_field("active", [FilterOperator::Eq])
        .allow_sort_field("completed_at")
        .allow_sort_field("issue_id")
        .with_default_sort(vec![SortClause::desc("completed_at")])
        .with_page_config(PaginationConfig::new(20, 100))
}

fn field_map() -> FieldMap {
    FieldMap::from_schema(&schema())
        .column("project_id", "project_id")
        .column("issue_id", "issue_id")
}

fn query(pairs: &[(&str, &str)]) -> ListQuery {
    parse_list_query(pairs.iter().copied(), &schema()).unwrap()
}

fn compiled(pairs: &[(&str, &str)], dialect: Dialect) -> awaken_query::SqlFragment {
    compile(
        &query(pairs),
        &field_map(),
        dialect,
        &CompileOptions::default(),
    )
    .unwrap()
}

#[test]
fn compiles_eq_and_comparison_postgres() {
    let fragment = compiled(
        &[
            ("filter[project_id]", "p-1"),
            ("filter[completed_at][gt]", "2026-06-01"),
            ("page[size]", "50"),
        ],
        Dialect::Postgres,
    );
    assert_eq!(
        fragment.where_clause.as_deref(),
        Some("project_id = $1 AND completed_at > $2")
    );
    assert_eq!(fragment.order_by.as_deref(), Some("completed_at DESC"));
    assert_eq!(fragment.limit, 50);
    assert_eq!(
        fragment.params,
        vec![
            FilterParam::String("p-1".to_string()),
            FilterParam::String("2026-06-01".to_string()),
        ]
    );
}

#[test]
fn placeholders_differ_by_dialect() {
    let pg = compiled(
        &[
            ("filter[project_id]", "p-1"),
            ("filter[completed_at][gt]", "x"),
        ],
        Dialect::Postgres,
    );
    let sqlite = compiled(
        &[
            ("filter[project_id]", "p-1"),
            ("filter[completed_at][gt]", "x"),
        ],
        Dialect::Sqlite,
    );
    assert_eq!(
        pg.where_clause.as_deref(),
        Some("project_id = $1 AND completed_at > $2")
    );
    assert_eq!(
        sqlite.where_clause.as_deref(),
        Some("project_id = ? AND completed_at > ?")
    );
    // Same parameters regardless of placeholder rendering.
    assert_eq!(pg.params, sqlite.params);
}

#[test]
fn set_membership_binds_array_on_postgres() {
    let fragment = compiled(&[("filter[project_id][in]", "a,b,c")], Dialect::Postgres);
    assert_eq!(
        fragment.where_clause.as_deref(),
        Some("project_id = ANY($1)")
    );
    assert_eq!(
        fragment.params,
        vec![FilterParam::Array(vec![
            FilterParam::String("a".to_string()),
            FilterParam::String("b".to_string()),
            FilterParam::String("c".to_string()),
        ])]
    );
}

#[test]
fn set_membership_expands_on_sqlite() {
    let fragment = compiled(&[("filter[project_id][in]", "a,b,c")], Dialect::Sqlite);
    assert_eq!(
        fragment.where_clause.as_deref(),
        Some("project_id IN (?, ?, ?)")
    );
    assert_eq!(fragment.params.len(), 3);
}

#[test]
fn not_in_negates_on_both_dialects() {
    let pg = compiled(&[("filter[project_id][not_in]", "a,b")], Dialect::Postgres);
    let sqlite = compiled(&[("filter[project_id][not_in]", "a,b")], Dialect::Sqlite);
    assert_eq!(pg.where_clause.as_deref(), Some("project_id != ALL($1)"));
    assert_eq!(
        sqlite.where_clause.as_deref(),
        Some("project_id NOT IN (?, ?)")
    );
}

#[test]
fn empty_set_is_dialect_safe_on_sqlite() {
    // An empty `in` matches nothing, an empty `not_in` matches everything;
    // SQLite has no `IN ()` syntax, so the builder emits constant predicates.
    let map = FieldMap::new().column("project_id", "project_id");
    let make = |operator| ListQuery {
        filter: FilterNode::Condition {
            field: "project_id".to_string(),
            operator,
            value: FilterValue::List(vec![]),
        },
        sort: vec![],
        page: PaginationRequest::cursor(20, None),
    };
    let in_q = compile(
        &make(FilterOperator::In),
        &map,
        Dialect::Sqlite,
        &CompileOptions::default(),
    )
    .unwrap();
    let nin_q = compile(
        &make(FilterOperator::NotIn),
        &map,
        Dialect::Sqlite,
        &CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(in_q.where_clause.as_deref(), Some("1 = 0"));
    assert_eq!(nin_q.where_clause.as_deref(), Some("1 = 1"));
    assert!(in_q.params.is_empty());
}

#[test]
fn contains_wraps_and_escapes_pattern() {
    let fragment = compiled(&[("filter[title][contains]", "50%_off")], Dialect::Postgres);
    assert_eq!(
        fragment.where_clause.as_deref(),
        Some(r"title ILIKE $1 ESCAPE '\'")
    );
    assert_eq!(
        fragment.params,
        vec![FilterParam::String(r"%50\%\_off%".to_string())]
    );
}

#[test]
fn startswith_uses_like_on_sqlite() {
    let fragment = compiled(&[("filter[name][starts_with]", "ab")], Dialect::Sqlite);
    assert_eq!(
        fragment.where_clause.as_deref(),
        Some(r"name LIKE ? ESCAPE '\'")
    );
    assert_eq!(
        fragment.params,
        vec![FilterParam::String("ab%".to_string())]
    );
}

#[test]
fn ilike_maps_to_like_on_sqlite_but_ilike_on_postgres() {
    let pg = compiled(&[("filter[name][ilike]", "a%")], Dialect::Postgres);
    let sqlite = compiled(&[("filter[name][ilike]", "a%")], Dialect::Sqlite);
    assert_eq!(pg.where_clause.as_deref(), Some("name ILIKE $1"));
    assert_eq!(sqlite.where_clause.as_deref(), Some("name LIKE ?"));
    // Raw LIKE/ILIKE values are not auto-escaped: the caller's wildcards stand.
    assert_eq!(pg.params, vec![FilterParam::String("a%".to_string())]);
}

#[test]
fn null_eq_becomes_is_null() {
    let pg = compiled(&[("filter[archived_at]", "null")], Dialect::Postgres);
    assert_eq!(pg.where_clause.as_deref(), Some("archived_at IS NULL"));
    assert!(pg.params.is_empty());

    let ne = compiled(&[("filter[archived_at][ne]", "null")], Dialect::Postgres);
    assert_eq!(ne.where_clause.as_deref(), Some("archived_at IS NOT NULL"));
}

#[test]
fn typed_scalars_round_trip() {
    let fragment = compiled(&[("filter[active]", "true")], Dialect::Postgres);
    assert_eq!(fragment.params, vec![FilterParam::Bool(true)]);
}

#[test]
fn or_and_nested_groups_are_parenthesized() {
    // parse_list_query only builds AND groups, so construct an OR tree directly.
    let filter = FilterNode::Group {
        operator: BoolOperator::And,
        conditions: vec![
            FilterNode::Condition {
                field: "active".to_string(),
                operator: FilterOperator::Eq,
                value: FilterValue::Scalar(FilterScalar::Bool(true)),
            },
            FilterNode::Group {
                operator: BoolOperator::Or,
                conditions: vec![
                    FilterNode::Condition {
                        field: "project_id".to_string(),
                        operator: FilterOperator::Eq,
                        value: FilterValue::Scalar(FilterScalar::String("a".to_string())),
                    },
                    FilterNode::Condition {
                        field: "project_id".to_string(),
                        operator: FilterOperator::Eq,
                        value: FilterValue::Scalar(FilterScalar::String("b".to_string())),
                    },
                ],
            },
        ],
    };
    let query = ListQuery {
        filter,
        sort: vec![],
        page: PaginationRequest::cursor(20, None),
    };
    let fragment = compile(
        &query,
        &field_map(),
        Dialect::Postgres,
        &CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fragment.where_clause.as_deref(),
        Some("active = $1 AND (project_id = $2 OR project_id = $3)")
    );
}

#[test]
fn empty_filter_yields_no_where_clause() {
    let query = ListQuery {
        filter: FilterNode::empty_and(),
        sort: vec![],
        page: PaginationRequest::cursor(20, None),
    };
    let fragment = compile(
        &query,
        &field_map(),
        Dialect::Postgres,
        &CompileOptions::default(),
    )
    .unwrap();
    assert_eq!(fragment.where_clause, None);
    assert_eq!(fragment.order_by, None);
}

#[test]
fn start_param_offsets_postgres_placeholders() {
    let opts = CompileOptions {
        start_param: 4,
        ..CompileOptions::default()
    };
    let fragment = compile(
        &query(&[
            ("filter[project_id]", "p"),
            ("filter[completed_at][gt]", "x"),
        ]),
        &field_map(),
        Dialect::Postgres,
        &opts,
    )
    .unwrap();
    assert_eq!(
        fragment.where_clause.as_deref(),
        Some("project_id = $4 AND completed_at > $5")
    );
}

#[test]
fn unmapped_field_is_rejected() {
    // A schema field with no column mapping must fail closed, never pass through.
    let map = FieldMap::new(); // empty map
    let err = compile(
        &query(&[("filter[project_id]", "p")]),
        &map,
        Dialect::Postgres,
        &CompileOptions::default(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        QueryBuildError::UnmappedField("project_id".to_string())
    );
}

#[test]
fn array_bound_is_enforced() {
    let opts = CompileOptions {
        max_array_size: 2,
        ..CompileOptions::default()
    };
    let err = compile(
        &query(&[("filter[project_id][in]", "a,b,c")]),
        &field_map(),
        Dialect::Postgres,
        &opts,
    )
    .unwrap_err();
    assert_eq!(
        err,
        QueryBuildError::ArrayTooLarge {
            field: "project_id".to_string(),
            size: 3,
            max: 2
        }
    );
}

#[test]
fn string_bound_is_enforced() {
    let opts = CompileOptions {
        max_string_len: 3,
        ..CompileOptions::default()
    };
    let err = compile(
        &query(&[("filter[project_id]", "toolong")]),
        &field_map(),
        Dialect::Postgres,
        &opts,
    )
    .unwrap_err();
    assert_eq!(
        err,
        QueryBuildError::StringTooLong {
            field: "project_id".to_string(),
            len: 7,
            max: 3
        }
    );
}

#[test]
fn value_shape_mismatch_is_rejected() {
    // `in` with a scalar value (constructed directly) is a shape error.
    let map = field_map();
    let query = ListQuery {
        filter: FilterNode::Condition {
            field: "project_id".to_string(),
            operator: FilterOperator::In,
            value: FilterValue::Scalar(FilterScalar::String("a".to_string())),
        },
        sort: vec![],
        page: PaginationRequest::cursor(20, None),
    };
    let err = compile(&query, &map, Dialect::Postgres, &CompileOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        QueryBuildError::ValueShapeMismatch {
            operator: FilterOperator::In,
            ..
        }
    ));
}

#[test]
fn multi_field_sort_renders_directions() {
    let fragment = compiled(&[("sort", "-completed_at,issue_id")], Dialect::Postgres);
    assert_eq!(
        fragment.order_by.as_deref(),
        Some("completed_at DESC, issue_id ASC")
    );
}
