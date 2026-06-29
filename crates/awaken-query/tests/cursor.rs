//! Opaque cursor encode/decode and keyset condition building.

use awaken_api_contract::SortClause;
use awaken_query::{
    CursorState, Dialect, FieldMap, FilterParam, QueryBuildError, keyset_condition,
};

fn map() -> FieldMap {
    FieldMap::new()
        .column("completed_at", "completed_at")
        .column("id", "id")
}

fn sort() -> Vec<SortClause> {
    vec![SortClause::desc("completed_at"), SortClause::asc("id")]
}

fn cursor() -> CursorState {
    CursorState::new()
        .set("completed_at", "2026-06-01")
        .set("id", 42)
}

#[test]
fn cursor_round_trips_through_opaque_token() {
    let state = cursor();
    let token = state.encode();
    // Opaque: not the raw values.
    assert!(!token.contains("2026"));
    let decoded = CursorState::decode(&token).unwrap();
    assert_eq!(decoded, state);
}

#[test]
fn decode_rejects_garbage() {
    assert!(matches!(
        CursorState::decode(""),
        Err(QueryBuildError::InvalidCursor(_))
    ));
    assert!(matches!(
        CursorState::decode("!!!not-base64!!!"),
        Err(QueryBuildError::InvalidCursor(_))
    ));
    // Valid base64 of a non-object JSON value.
    let array = awaken_query_test_encode(b"[1,2,3]");
    assert!(matches!(
        CursorState::decode(&array),
        Err(QueryBuildError::InvalidCursor(_))
    ));
}

fn awaken_query_test_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[test]
fn forward_keyset_is_compound_row_value_postgres() {
    let (sql, params) =
        keyset_condition(&sort(), &cursor(), &map(), Dialect::Postgres, 1, false).unwrap();
    assert_eq!(
        sql,
        "(completed_at < $1 OR (completed_at = $2 AND id > $3))"
    );
    assert_eq!(
        params,
        vec![
            FilterParam::String("2026-06-01".to_string()),
            FilterParam::String("2026-06-01".to_string()),
            FilterParam::Number(42.0),
        ]
    );
}

#[test]
fn forward_keyset_uses_anonymous_placeholders_sqlite() {
    let (sql, _) = keyset_condition(&sort(), &cursor(), &map(), Dialect::Sqlite, 1, false).unwrap();
    assert_eq!(sql, "(completed_at < ? OR (completed_at = ? AND id > ?))");
}

#[test]
fn backward_keyset_flips_every_comparison() {
    let (sql, _) =
        keyset_condition(&sort(), &cursor(), &map(), Dialect::Postgres, 1, true).unwrap();
    // desc->`>` and asc->`<` once direction is reversed.
    assert_eq!(
        sql,
        "(completed_at > $1 OR (completed_at = $2 AND id < $3))"
    );
}

#[test]
fn keyset_respects_start_param() {
    let (sql, _) =
        keyset_condition(&sort(), &cursor(), &map(), Dialect::Postgres, 10, false).unwrap();
    assert_eq!(
        sql,
        "(completed_at < $10 OR (completed_at = $11 AND id > $12))"
    );
}

#[test]
fn missing_sort_value_fails_closed() {
    let partial = CursorState::new().set("completed_at", "2026-06-01");
    let err = keyset_condition(&sort(), &partial, &map(), Dialect::Postgres, 1, false).unwrap_err();
    assert!(matches!(err, QueryBuildError::InvalidCursor(_)));
}

#[test]
fn empty_sort_is_rejected() {
    let err = keyset_condition(&[], &cursor(), &map(), Dialect::Postgres, 1, false).unwrap_err();
    assert!(matches!(err, QueryBuildError::InvalidCursor(_)));
}

#[test]
fn unmapped_sort_field_is_rejected() {
    let bare = FieldMap::new().column("completed_at", "completed_at");
    let err = keyset_condition(&sort(), &cursor(), &bare, Dialect::Postgres, 1, false).unwrap_err();
    assert_eq!(err, QueryBuildError::UnmappedField("id".to_string()));
}
