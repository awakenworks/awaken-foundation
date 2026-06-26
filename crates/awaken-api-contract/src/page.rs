use serde::{Deserialize, Serialize};

/// Pagination mode for one list request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PaginationMode {
    /// Opaque keyset cursor pagination.
    Cursor,
    /// One-based page-number pagination.
    Page,
    /// Zero-based offset pagination.
    Offset,
}

/// Pagination limits and defaults for one API surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaginationConfig {
    pub default_size: u32,
    pub max_size: u32,
    pub default_mode: PaginationMode,
}

impl PaginationConfig {
    #[must_use]
    pub const fn new(default_size: u32, max_size: u32) -> Self {
        Self {
            default_size,
            max_size,
            default_mode: PaginationMode::Cursor,
        }
    }

    #[must_use]
    pub const fn with_default_mode(mut self, default_mode: PaginationMode) -> Self {
        self.default_mode = default_mode;
        self
    }
}

impl Default for PaginationConfig {
    fn default() -> Self {
        Self {
            default_size: 20,
            max_size: 200,
            default_mode: PaginationMode::Cursor,
        }
    }
}

/// Backwards-compatible name for cursor-first APIs.
pub type CursorPageConfig = PaginationConfig;

/// Client request for an opaque cursor page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CursorPageRequest {
    pub size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl CursorPageRequest {
    #[must_use]
    pub fn first(size: u32) -> Self {
        Self { size, cursor: None }
    }

    #[must_use]
    pub fn after(size: u32, cursor: impl Into<String>) -> Self {
        Self {
            size,
            cursor: Some(cursor.into()),
        }
    }
}

/// Client request for page-number pagination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PageNumberRequest {
    pub size: u32,
    /// One-based page number.
    pub number: u32,
}

impl PageNumberRequest {
    #[must_use]
    pub const fn new(size: u32, number: u32) -> Self {
        Self { size, number }
    }
}

/// Client request for offset pagination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OffsetPageRequest {
    pub size: u32,
    /// Zero-based item offset.
    pub offset: u64,
}

impl OffsetPageRequest {
    #[must_use]
    pub const fn new(size: u32, offset: u64) -> Self {
        Self { size, offset }
    }
}

/// Pagination request accepted by list APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PaginationRequest {
    Cursor(CursorPageRequest),
    Page(PageNumberRequest),
    Offset(OffsetPageRequest),
}

impl PaginationRequest {
    #[must_use]
    pub fn cursor(size: u32, cursor: Option<String>) -> Self {
        Self::Cursor(CursorPageRequest { size, cursor })
    }

    #[must_use]
    pub const fn page(size: u32, number: u32) -> Self {
        Self::Page(PageNumberRequest { size, number })
    }

    #[must_use]
    pub const fn offset(size: u32, offset: u64) -> Self {
        Self::Offset(OffsetPageRequest { size, offset })
    }

    #[must_use]
    pub const fn size(&self) -> u32 {
        match self {
            Self::Cursor(request) => request.size,
            Self::Page(request) => request.size,
            Self::Offset(request) => request.size,
        }
    }

    #[must_use]
    pub fn cursor_value(&self) -> Option<&str> {
        match self {
            Self::Cursor(request) => request.cursor.as_deref(),
            Self::Page(_) | Self::Offset(_) => None,
        }
    }
}

/// Generic cursor page envelope for APIs that choose a neutral `items` shape.
///
/// Product APIs may still expose named item fields such as `issues` or `runs`;
/// in that case this type is the reference shape for the page metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CursorPage<T> {
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

impl<T> CursorPage<T> {
    #[must_use]
    pub fn new(items: Vec<T>, cursor: Option<String>) -> Self {
        Self { items, cursor }
    }
}

/// Generic page-number page envelope for APIs that need traditional pagination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NumberedPage<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_items: Option<u64>,
}

impl<T> NumberedPage<T> {
    #[must_use]
    pub fn new(items: Vec<T>, page: u32, size: u32, total_items: Option<u64>) -> Self {
        Self {
            items,
            page,
            size,
            total_items,
        }
    }
}
