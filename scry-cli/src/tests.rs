use super::display::*;
use super::*;
use scry_client::ResultEntry;

#[test]
fn sort_names_map_onto_orders() {
    assert_eq!(parse_order("recent"), Some(Order::Recent));
    assert_eq!(parse_order("size"), Some(Order::Largest));
    assert_eq!(parse_order("relevance"), Some(Order::Relevance));
    assert_eq!(parse_order("largest"), None);
}

#[test]
fn infers_wildcard_from_glob_metacharacters() {
    assert_eq!(infer_query_kind(None, "*.pdf"), QueryKind::Wildcard);
    assert_eq!(infer_query_kind(None, "report.*"), QueryKind::Wildcard);
    assert_eq!(infer_query_kind(None, "report?.pdf"), QueryKind::Wildcard);
}

#[test]
fn defaults_plain_queries_to_path_terms() {
    assert_eq!(
        infer_query_kind(None, "project notes"),
        QueryKind::PathTerms
    );
}

#[test]
fn explicit_kind_takes_precedence() {
    assert_eq!(
        infer_query_kind(Some(QueryKind::Prefix), "*.pdf"),
        QueryKind::Prefix
    );
}

#[test]
fn sizes_distinguish_exact_lower_bound_and_unknown() {
    let entry = |size, is_dir, size_exact| ResultEntry {
        path: "item".into(),
        size,
        mtime: 0,
        is_dir,
        size_exact,
    };
    assert_eq!(display_size(&entry(42, false, true)), "42 B");
    assert_eq!(display_size(&entry(0, false, true)), "0 B");
    assert_eq!(display_size(&entry(42, true, false)), "≥42 B");
    assert_eq!(display_size(&entry(0, false, false)), "—");
    assert_eq!(human_size(1_023), "1023 B");
    assert_eq!(human_size(1_024), "1 KB");
    assert_eq!(human_size(1_572_864), "1.5 MB");
}

#[test]
fn terminal_text_cannot_inject_control_sequences() {
    assert_eq!(visible_terminal_text("safe\u{1b}[2J"), "safe�[2J");
}
