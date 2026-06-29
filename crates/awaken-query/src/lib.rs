//! Dialect-aware compiler from `awaken-api-contract` list queries to
//! parameterized SQL fragments.
//!
//! This crate is the **execution half** of list queries; `awaken-api-contract`
//! is the contract half. It takes an *already-validated* [`ListQuery`] (the
//! contract crate's `QuerySchema` allowlist is the single validation path) plus
//! an author-controlled [`FieldMap`], and emits a [`SqlFragment`] — a `WHERE`
//! body, an `ORDER BY` body, a limit, and ordered bound [`FilterParam`]s. It
//! also builds opaque keyset [`CursorState`] conditions and grouped-pagination
//! fragments.
//!
//! It never opens a connection or executes SQL: the consumer owns the
//! connection, the `FROM`/projection, tenancy/RLS predicates, and result
//! mapping.
//!
//! # Security invariants
//!
//! - Every value is a **bound parameter**, never interpolated.
//! - Every field is **mapped or rejected**: identifiers come from the
//!   [`FieldMap`], never from request text.
//! - Array and string **bounds** ([`CompileOptions`]) are checked before any
//!   SQL is produced.
//! - The optional string DSL ([`dsl`]) and structured input share one
//!   validation path; there is no second, unguarded route to the builder.
//! - Cursors are opaque and fail closed on a missing/garbled sort key.
//!
//! # Example
//!
//! ```
//! use awaken_api_contract::{FilterOperator, PaginationConfig, QuerySchema, SortClause, parse_list_query};
//! use awaken_query::{CompileOptions, Dialect, FieldMap, compile};
//!
//! let schema = QuerySchema::new()
//!     .allow_filter_field("project_id", [FilterOperator::Eq])
//!     .allow_filter_field("completed_at", [FilterOperator::Gt])
//!     .allow_sort_field("completed_at")
//!     .with_default_sort(vec![SortClause::desc("completed_at")])
//!     .with_page_config(PaginationConfig::new(20, 100));
//!
//! let query = parse_list_query(
//!     [
//!         ("filter[project_id]", "p-1"),
//!         ("filter[completed_at][gt]", "2026-06-01"),
//!         ("page[size]", "50"),
//!     ],
//!     &schema,
//! )
//! .unwrap();
//!
//! let map = FieldMap::from_schema(&schema).column("completed_at", "completed_at");
//! let fragment = compile(&query, &map, Dialect::Postgres, &CompileOptions::default()).unwrap();
//!
//! assert_eq!(
//!     fragment.where_clause.as_deref(),
//!     Some("project_id = $1 AND completed_at > $2"),
//! );
//! assert_eq!(fragment.order_by.as_deref(), Some("completed_at DESC"));
//! assert_eq!(fragment.limit, 50);
//! ```

mod build;
mod cursor;
mod dialect;
mod group;

#[cfg(feature = "dsl")]
pub mod dsl;

pub use build::{
    CompileOptions, FieldMap, FilterParam, QueryBuildError, SqlFragment, ValueShape, compile,
};
pub use cursor::{CursorState, keyset_condition};
pub use dialect::{Dialect, ParamBuilder};
pub use group::{GroupedQuery, GroupedSqlFragment, compile_grouped};

/// Default maximum element count in an `in` / `not_in` set.
pub const DEFAULT_MAX_ARRAY_SIZE: usize = 100;

/// Default maximum length of a string value.
pub const DEFAULT_MAX_STRING_LENGTH: usize = 1000;
