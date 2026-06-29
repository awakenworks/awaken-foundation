//! SQL dialect rendering.
//!
//! The builder is written once; the `Dialect` renders the handful of things
//! that actually differ between backends — placeholder syntax, set membership,
//! and case-insensitive matching. Adding a backend is a new variant here, not a
//! second builder.

use crate::build::FilterParam;

/// Target SQL dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// PostgreSQL: `$N` placeholders, `= ANY` / `!= ALL` set membership,
    /// native `ILIKE`.
    Postgres,
    /// SQLite: `?` placeholders, `IN (…)` / `NOT IN (…)` set membership,
    /// `LIKE` (ASCII case-insensitive) for both `like` and `ilike`.
    Sqlite,
}

impl Dialect {
    /// Render the placeholder for the `index`-th bound parameter (1-based).
    /// Postgres is positional (`$index`); SQLite uses the anonymous `?`, so the
    /// index is consumed only to keep call sites uniform.
    #[must_use]
    pub fn placeholder(self, index: usize) -> String {
        match self {
            Self::Postgres => format!("${index}"),
            Self::Sqlite => "?".to_string(),
        }
    }

    /// Keyword for case-insensitive `LIKE`. SQLite's `LIKE` is already
    /// case-insensitive for ASCII, so it has no separate `ILIKE`.
    #[must_use]
    pub const fn ilike_keyword(self) -> &'static str {
        match self {
            Self::Postgres => "ILIKE",
            Self::Sqlite => "LIKE",
        }
    }

    /// Whether this dialect binds an `IN` set as a single array parameter
    /// (Postgres `= ANY($1)`) rather than expanding to one placeholder per
    /// element (SQLite `IN (?, ?, …)`).
    #[must_use]
    pub const fn binds_set_as_array(self) -> bool {
        matches!(self, Self::Postgres)
    }

    /// Accumulator for bound parameters and their placeholders. Centralizes
    /// numbering so every call site renders placeholders the same way.
    #[must_use]
    pub fn params(self, start_param: usize) -> ParamBuilder {
        ParamBuilder {
            dialect: self,
            params: Vec::new(),
            next: start_param.max(1),
        }
    }
}

/// Collects bound parameters in placeholder order and hands back the matching
/// placeholder text for each.
#[derive(Debug)]
pub struct ParamBuilder {
    dialect: Dialect,
    params: Vec<FilterParam>,
    next: usize,
}

impl ParamBuilder {
    /// Bind one value and return its placeholder (`$N` or `?`).
    pub fn bind(&mut self, param: FilterParam) -> String {
        let placeholder = self.dialect.placeholder(self.next);
        self.params.push(param);
        self.next += 1;
        placeholder
    }

    /// The 1-based index the next bound parameter will take.
    #[must_use]
    pub const fn next_index(&self) -> usize {
        self.next
    }

    /// Consume the builder, yielding the parameters in placeholder order.
    #[must_use]
    pub fn into_params(self) -> Vec<FilterParam> {
        self.params
    }
}
