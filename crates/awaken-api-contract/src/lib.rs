//! Neutral API contract primitives.
//!
//! This crate carries shared wire mechanisms only: problem details,
//! pagination, sorting, allowlisted filter ASTs, and credential-reference
//! shapes. It intentionally does not define product resources, authorization
//! policy, database execution, secret storage, or an HTTP framework adapter.

pub mod auth;
pub mod error;
pub mod page;
pub mod query;

pub use auth::{
    ApiKeyLocation, AuthRequirement, AuthScheme, ConnectionAuth, CredentialRef, OAuth2Flow,
};
pub use error::{
    AI_SDK_UI_MESSAGE_STREAM_HEADER, AI_SDK_UI_MESSAGE_STREAM_V1, AgUiRunErrorEvent, AiSdkDataPart,
    AiSdkErrorPart, ApiError, ErrorSource, FieldError, PROBLEM_JSON_CONTENT_TYPE, ProblemType,
    ProblemTypeError, REQUEST_ID_HEADER,
};
pub use page::{
    CursorPage, CursorPageConfig, CursorPageRequest, NumberedPage, OffsetPageRequest,
    PageNumberRequest, PaginationConfig, PaginationMode, PaginationRequest,
};
pub use query::{
    BoolOperator, FilterNode, FilterOperator, FilterScalar, FilterValue, ListQuery, QueryError,
    QuerySchema, SortClause, SortDirection, parse_list_query,
};
