//! Optional string DSL — parses into the same AST and through the same allowlist.

#![cfg(feature = "dsl")]

use awaken_api_contract::{
    BoolOperator, FilterNode, FilterOperator, FilterScalar, FilterValue, QueryError, QuerySchema,
};
use awaken_query::dsl::{DslError, parse_filter_dsl};

fn schema() -> QuerySchema {
    QuerySchema::new()
        .allow_filter_field("name", [FilterOperator::Eq, FilterOperator::Like])
        .allow_filter_field("active", [FilterOperator::Eq])
        .allow_filter_field("age", [FilterOperator::Gte])
        .allow_filter_field("id", [FilterOperator::In, FilterOperator::NotIn])
        .allow_filter_field("title", [FilterOperator::Contains])
}

#[test]
fn parses_single_condition() {
    let node = parse_filter_dsl("name='Alice'", &schema()).unwrap();
    assert_eq!(
        node,
        FilterNode::Condition {
            field: "name".to_string(),
            operator: FilterOperator::Eq,
            value: FilterValue::Scalar(FilterScalar::String("Alice".to_string())),
        }
    );
}

#[test]
fn parses_typed_scalars() {
    let node = parse_filter_dsl("active=true", &schema()).unwrap();
    let FilterNode::Condition { value, .. } = node else {
        panic!("expected condition");
    };
    assert_eq!(value, FilterValue::Scalar(FilterScalar::Bool(true)));
}

#[test]
fn parses_and_or_precedence() {
    // `a & b | c` => OR(AND(a,b), c)
    let node = parse_filter_dsl("name='A' & active=true | age>=18", &schema()).unwrap();
    let FilterNode::Group {
        operator,
        conditions,
    } = &node
    else {
        panic!("expected group");
    };
    assert_eq!(*operator, BoolOperator::Or);
    assert_eq!(conditions.len(), 2);
    assert!(matches!(
        &conditions[0],
        FilterNode::Group {
            operator: BoolOperator::And,
            ..
        }
    ));
}

#[test]
fn parses_parenthesized_group() {
    let node = parse_filter_dsl("(name='A' | name='B') & active=true", &schema()).unwrap();
    let FilterNode::Group {
        operator,
        conditions,
    } = &node
    else {
        panic!("expected group");
    };
    assert_eq!(*operator, BoolOperator::And);
    assert_eq!(conditions.len(), 2);
    assert!(matches!(
        &conditions[0],
        FilterNode::Group {
            operator: BoolOperator::Or,
            ..
        }
    ));
}

#[test]
fn parses_in_and_not_in_sets() {
    let node = parse_filter_dsl("id @ ('a','b',3)", &schema()).unwrap();
    let FilterNode::Condition {
        operator, value, ..
    } = node
    else {
        panic!("expected condition");
    };
    assert_eq!(operator, FilterOperator::In);
    assert_eq!(
        value,
        FilterValue::List(vec![
            FilterScalar::String("a".to_string()),
            FilterScalar::String("b".to_string()),
            FilterScalar::Number(3.0),
        ])
    );

    let not_in = parse_filter_dsl("id !@ ('x')", &schema()).unwrap();
    let FilterNode::Condition { operator, .. } = not_in else {
        panic!("expected condition");
    };
    assert_eq!(operator, FilterOperator::NotIn);
}

#[test]
fn parses_word_operators() {
    let node = parse_filter_dsl("title contains 'urgent'", &schema()).unwrap();
    let FilterNode::Condition { operator, .. } = node else {
        panic!("expected condition");
    };
    assert_eq!(operator, FilterOperator::Contains);
}

#[test]
fn does_not_split_inside_quotes() {
    // The `&` and `|` inside the quoted literal must not split the expression.
    let node = parse_filter_dsl("name='a & b | c'", &schema()).unwrap();
    let FilterNode::Condition { value, .. } = node else {
        panic!("expected single condition");
    };
    assert_eq!(
        value,
        FilterValue::Scalar(FilterScalar::String("a & b | c".to_string()))
    );
}

#[test]
fn routes_through_the_same_allowlist() {
    // Unknown field is rejected by QuerySchema, not silently accepted.
    let err = parse_filter_dsl("secret='x'", &schema()).unwrap_err();
    assert_eq!(
        err,
        DslError::Validation(QueryError::UnsupportedFilterField("secret".to_string()))
    );
}

#[test]
fn rejects_disallowed_operator_via_allowlist() {
    // `name` allows Eq/Like but not Gte.
    let err = parse_filter_dsl("name>=5", &schema()).unwrap_err();
    assert!(matches!(
        err,
        DslError::Validation(QueryError::OperatorNotAllowed { .. })
    ));
}

#[test]
fn rejects_unbalanced_parentheses() {
    let err = parse_filter_dsl("(name='A'", &schema()).unwrap_err();
    assert_eq!(err, DslError::UnbalancedParentheses);
}

#[test]
fn rejects_missing_operator() {
    let err = parse_filter_dsl("justfield", &schema()).unwrap_err();
    assert!(matches!(err, DslError::Syntax(_)));
}
