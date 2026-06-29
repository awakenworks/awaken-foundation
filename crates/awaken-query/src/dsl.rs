//! Optional string filter grammar (feature `dsl`).
//!
//! A compact infix syntax for CLI, saved views, and expert search boxes. It is
//! **not** the canonical wire syntax — it parses into the same
//! [`FilterNode`](awaken_api_contract::FilterNode) AST and runs through the
//! same `QuerySchema` allowlist as bracket filters, so it has no independent
//! path to the SQL builder and cannot bypass field/operator validation.
//!
//! # Grammar
//!
//! - equality / comparison: `name='Alice'`, `age>=18`, `price<100`
//! - logical and / or: `name='Alice' & active=true`, `a=1 | b=2`
//! - parenthesized groups: `(a=1 | b=2) & active=true`
//! - set membership: `id @ ('a','b')`, `id !@ ('a','b')`
//! - pattern match: `name ~ 'a%'` (LIKE), `name ~~ 'a%'` (ILIKE)
//! - substring helpers: `name contains 'ab'`, `name startswith 'a'`,
//!   `name endswith 'z'`
//! - values: single-quoted strings, numbers, `true`, `false`, `null`

use awaken_api_contract::{
    BoolOperator, FilterNode, FilterOperator, FilterScalar, FilterValue, QueryError, QuerySchema,
};

/// String DSL parse or validation failure.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum DslError {
    #[error("invalid filter syntax: {0}")]
    Syntax(String),
    #[error("unbalanced parentheses")]
    UnbalancedParentheses,
    #[error(transparent)]
    Validation(#[from] QueryError),
}

/// Parse a filter string into a `FilterNode` and validate it against `schema`.
pub fn parse_filter_dsl(input: &str, schema: &QuerySchema) -> Result<FilterNode, DslError> {
    let node = parse_or(input.trim())?;
    schema.validate_filter(&node)?;
    Ok(node)
}

fn parse_or(input: &str) -> Result<FilterNode, DslError> {
    let parts = split_top_level(input, '|')?;
    if parts.len() == 1 {
        return parse_and(parts[0].trim());
    }
    let conditions = parts
        .iter()
        .map(|part| parse_and(part.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FilterNode::Group {
        operator: BoolOperator::Or,
        conditions,
    })
}

fn parse_and(input: &str) -> Result<FilterNode, DslError> {
    let parts = split_top_level(input, '&')?;
    if parts.len() == 1 {
        return parse_primary(parts[0].trim());
    }
    let conditions = parts
        .iter()
        .map(|part| parse_primary(part.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FilterNode::Group {
        operator: BoolOperator::And,
        conditions,
    })
}

fn parse_primary(input: &str) -> Result<FilterNode, DslError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(DslError::Syntax("empty expression".to_string()));
    }
    if let Some(inner) = input.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        return parse_or(inner.trim());
    }
    parse_condition(input)
}

fn parse_condition(input: &str) -> Result<FilterNode, DslError> {
    let (field_end, op_len, operator) = find_operator(input)
        .ok_or_else(|| DslError::Syntax(format!("no operator in condition '{input}'")))?;
    let field = input[..field_end].trim();
    let raw_value = input[field_end + op_len..].trim();
    if field.is_empty() {
        return Err(DslError::Syntax(format!("missing field in '{input}'")));
    }

    let value = if matches!(operator, FilterOperator::In | FilterOperator::NotIn) {
        FilterValue::List(parse_set(raw_value)?)
    } else {
        FilterValue::Scalar(parse_scalar(raw_value))
    };

    Ok(FilterNode::Condition {
        field: field.to_string(),
        operator,
        value,
    })
}

/// Operators, longest match first so `>=` beats `>` and `!@` beats `@`.
const SYMBOL_OPERATORS: &[(&str, FilterOperator)] = &[
    ("!@", FilterOperator::NotIn),
    (">=", FilterOperator::Gte),
    ("<=", FilterOperator::Lte),
    ("!=", FilterOperator::Ne),
    ("~~", FilterOperator::Ilike),
    ("@", FilterOperator::In),
    ("~", FilterOperator::Like),
    (">", FilterOperator::Gt),
    ("<", FilterOperator::Lt),
    ("=", FilterOperator::Eq),
];

const WORD_OPERATORS: &[(&str, FilterOperator)] = &[
    (" contains ", FilterOperator::Contains),
    (" startswith ", FilterOperator::StartsWith),
    (" endswith ", FilterOperator::EndsWith),
];

/// Find the first top-level operator (outside quotes), returning the byte index
/// where the field ends, the operator's byte length, and the operator.
fn find_operator(input: &str) -> Option<(usize, usize, FilterOperator)> {
    let bytes = input.as_bytes();
    let mut in_quote = false;
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b'\'' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if !in_quote {
            for (token, operator) in WORD_OPERATORS {
                if input[i..].starts_with(token) {
                    return Some((i, token.len(), *operator));
                }
            }
            for (token, operator) in SYMBOL_OPERATORS {
                if input[i..].starts_with(token) {
                    return Some((i, token.len(), *operator));
                }
            }
        }
        i += 1;
    }
    None
}

/// Split on `delimiter` at paren depth zero and outside quotes.
fn split_top_level(input: &str, delimiter: char) -> Result<Vec<String>, DslError> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    let mut in_quote = false;

    for ch in input.chars() {
        match ch {
            '\'' => {
                in_quote = !in_quote;
                current.push(ch);
            }
            '(' if !in_quote => {
                depth += 1;
                current.push(ch);
            }
            ')' if !in_quote => {
                depth -= 1;
                if depth < 0 {
                    return Err(DslError::UnbalancedParentheses);
                }
                current.push(ch);
            }
            c if c == delimiter && depth == 0 && !in_quote => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if in_quote || depth != 0 {
        return Err(DslError::UnbalancedParentheses);
    }
    parts.push(current);
    Ok(parts)
}

fn parse_set(raw: &str) -> Result<Vec<FilterScalar>, DslError> {
    let inner = raw
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| DslError::Syntax(format!("set must be parenthesized: '{raw}'")))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(split_top_level(inner, ',')?
        .iter()
        .map(|item| parse_scalar(item.trim()))
        .collect())
}

fn parse_scalar(raw: &str) -> FilterScalar {
    if let Some(text) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return FilterScalar::String(text.to_string());
    }
    match raw {
        "true" => FilterScalar::Bool(true),
        "false" => FilterScalar::Bool(false),
        "null" => FilterScalar::Null,
        _ => raw.parse::<f64>().map_or_else(
            |_| FilterScalar::String(raw.to_string()),
            FilterScalar::Number,
        ),
    }
}
