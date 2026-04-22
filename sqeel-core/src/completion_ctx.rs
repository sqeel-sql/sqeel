//! SQL context detection for autocomplete.
//!
//! Given source text and a cursor byte offset, classifies what kind of
//! identifier the user is likely typing so the completion pool can be
//! narrowed to tables, columns, or children of a qualified identifier.
//!
//! This is a deliberately lightweight heuristic — it tokenizes words and
//! looks at the surrounding keywords. It does not build a full parse tree.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionCtx {
    /// Cursor sits immediately after `ident.` — complete with children of
    /// `parent` (tables if it's a database, columns if it's a table).
    Qualified { parent: String },
    /// Cursor follows a keyword that expects a table name
    /// (FROM / JOIN / INTO / UPDATE / DROP TABLE / ...).
    Table,
    /// Cursor is somewhere a column is expected. `tables` lists tables
    /// visible in the current statement (from FROM / JOIN clauses) to scope
    /// column suggestions when possible.
    Column { tables: Vec<String> },
    /// Unknown — any identifier may be valid.
    Any,
}

/// Parse the completion context at `byte_offset` inside `source`.
pub fn parse_context(source: &str, byte_offset: usize) -> CompletionCtx {
    let offset = byte_offset.min(source.len());
    let stmt_start = statement_start(source, offset);
    let stmt = &source[stmt_start..offset];

    // Strip the word-in-progress (alnum/_) so we look at what comes *before*
    // the prefix the user is actively typing.
    let trimmed = trim_trailing_word(stmt);

    if let Some(before_dot) = trimmed.strip_suffix('.') {
        let parent = trailing_ident(before_dot);
        if !parent.is_empty() {
            return CompletionCtx::Qualified { parent };
        }
    }

    let tokens = tokenize_words(stmt);
    classify(stmt, &tokens)
}

/// Byte offset of the start of the statement containing `offset`.
/// Walks back to the last unquoted `;` (or to 0 if none).
fn statement_start(source: &str, offset: usize) -> usize {
    let bytes = source.as_bytes();
    let mut last = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < offset && i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b';' if !in_single && !in_double => last = i + 1,
            _ => {}
        }
        i += 1;
    }
    last
}

fn trim_trailing_word(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    while end > 0 && is_ident_byte(bytes[end - 1]) {
        end -= 1;
    }
    &s[..end]
}

fn trailing_ident(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut start = bytes.len();
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    s[start..].to_string()
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[derive(Debug, Clone)]
struct Token<'a> {
    text: &'a str,
    upper: String,
    /// Byte offset where this token starts in the source slice.
    start: usize,
    /// Byte offset one past the token's last byte.
    end: usize,
}

fn tokenize_words(s: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' if !in_double => {
                in_single = !in_single;
                i += 1;
            }
            b'"' if !in_single => {
                in_double = !in_double;
                i += 1;
            }
            _ if in_single || in_double => {
                i += 1;
            }
            _ if is_ident_byte(c) => {
                let start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                let text = &s[start..i];
                out.push(Token {
                    text,
                    upper: text.to_ascii_uppercase(),
                    start,
                    end: i,
                });
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// True if the only byte between `a.end` and `b.start` in `source` is a `.`.
fn joined_by_dot(source: &str, a: &Token<'_>, b: &Token<'_>) -> bool {
    a.end < b.start && source[a.end..b.start].trim() == "."
}

fn is_table_keyword(tokens: &[Token<'_>], idx: usize) -> bool {
    match tokens[idx].upper.as_str() {
        "FROM" | "JOIN" | "INTO" | "UPDATE" | "DESCRIBE" | "DESC" => true,
        "TABLE" => {
            idx > 0
                && matches!(
                    tokens[idx - 1].upper.as_str(),
                    "DROP" | "ALTER" | "TRUNCATE" | "CREATE" | "RENAME"
                )
        }
        _ => false,
    }
}

fn is_column_keyword(tokens: &[Token<'_>], idx: usize) -> bool {
    match tokens[idx].upper.as_str() {
        "SELECT" | "WHERE" | "HAVING" | "ON" | "SET" | "AND" | "OR" => true,
        "BY" => {
            idx > 0
                && matches!(
                    tokens[idx - 1].upper.as_str(),
                    "ORDER" | "GROUP" | "PARTITION"
                )
        }
        _ => false,
    }
}

fn classify(source: &str, tokens: &[Token<'_>]) -> CompletionCtx {
    if tokens.is_empty() {
        return CompletionCtx::Any;
    }
    for i in (0..tokens.len()).rev() {
        if is_table_keyword(tokens, i) {
            return CompletionCtx::Table;
        }
        if is_column_keyword(tokens, i) {
            return CompletionCtx::Column {
                tables: collect_tables(source, tokens),
            };
        }
    }
    CompletionCtx::Any
}

fn collect_tables(source: &str, tokens: &[Token<'_>]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if is_table_keyword(tokens, i) && i + 1 < tokens.len() {
            // `FROM db.users` — skip past the `db.` qualifier to the
            // underlying table name, so column-scope lookups find it.
            let mut tbl_idx = i + 1;
            while tbl_idx + 1 < tokens.len()
                && joined_by_dot(source, &tokens[tbl_idx], &tokens[tbl_idx + 1])
            {
                tbl_idx += 1;
            }
            out.push(tokens[tbl_idx].text.to_string());
            i = tbl_idx + 1;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> CompletionCtx {
        parse_context(text, text.len())
    }

    #[test]
    fn empty_is_any() {
        assert_eq!(parse(""), CompletionCtx::Any);
    }

    #[test]
    fn bare_word_is_any() {
        assert_eq!(parse("sel"), CompletionCtx::Any);
    }

    #[test]
    fn from_expects_table() {
        assert_eq!(parse("select * from "), CompletionCtx::Table);
        assert_eq!(parse("select * from us"), CompletionCtx::Table);
    }

    #[test]
    fn join_expects_table() {
        assert_eq!(parse("select * from a join "), CompletionCtx::Table);
        assert_eq!(parse("select * from a left join "), CompletionCtx::Table);
    }

    #[test]
    fn insert_update_delete_table() {
        assert_eq!(parse("insert into "), CompletionCtx::Table);
        assert_eq!(parse("update "), CompletionCtx::Table);
        assert_eq!(parse("delete from "), CompletionCtx::Table);
    }

    #[test]
    fn drop_table_is_table_ctx() {
        assert_eq!(parse("drop table "), CompletionCtx::Table);
        assert_eq!(parse("truncate table "), CompletionCtx::Table);
        assert_eq!(parse("alter table "), CompletionCtx::Table);
        // Bare "table " (no DROP/ALTER) is not a table keyword.
        assert_ne!(parse("table "), CompletionCtx::Table);
    }

    #[test]
    fn describe_is_table_ctx() {
        assert_eq!(parse("describe "), CompletionCtx::Table);
        assert_eq!(parse("desc "), CompletionCtx::Table);
    }

    #[test]
    fn qualified_after_dot() {
        assert_eq!(
            parse("select * from deepci_maindb."),
            CompletionCtx::Qualified {
                parent: "deepci_maindb".into()
            }
        );
    }

    #[test]
    fn qualified_with_partial_prefix() {
        assert_eq!(
            parse("select * from deepci_maindb.us"),
            CompletionCtx::Qualified {
                parent: "deepci_maindb".into()
            }
        );
    }

    #[test]
    fn qualified_on_alias() {
        assert_eq!(
            parse("select u."),
            CompletionCtx::Qualified { parent: "u".into() }
        );
    }

    #[test]
    fn column_after_select() {
        match parse("select ") {
            CompletionCtx::Column { tables } => assert!(tables.is_empty()),
            other => panic!("expected Column, got {other:?}"),
        }
    }

    #[test]
    fn column_after_where_has_tables() {
        match parse("select * from users where ") {
            CompletionCtx::Column { tables } => assert_eq!(tables, vec!["users".to_string()]),
            other => panic!("expected Column, got {other:?}"),
        }
    }

    #[test]
    fn column_scope_picks_up_joins() {
        match parse("select * from a join b on ") {
            CompletionCtx::Column { tables } => {
                assert_eq!(tables, vec!["a".to_string(), "b".to_string()])
            }
            other => panic!("expected Column, got {other:?}"),
        }
    }

    #[test]
    fn column_strips_db_prefix_on_tables() {
        match parse("select * from mydb.users where ") {
            CompletionCtx::Column { tables } => assert_eq!(tables, vec!["users".to_string()]),
            other => panic!("expected Column, got {other:?}"),
        }
    }

    #[test]
    fn order_by_is_column_ctx() {
        match parse("select * from t order by ") {
            CompletionCtx::Column { tables } => assert_eq!(tables, vec!["t".to_string()]),
            other => panic!("expected Column, got {other:?}"),
        }
    }

    #[test]
    fn statement_boundary_resets_context() {
        // The `select x from t;` closes the first statement; cursor ctx
        // should be driven by the second statement only.
        assert_eq!(parse("select x from t; drop table "), CompletionCtx::Table);
    }

    #[test]
    fn qualifier_wins_over_keyword_context() {
        // Even after FROM, a dot means we want children of the preceding
        // identifier, not a generic table list.
        assert_eq!(
            parse("select * from mydb."),
            CompletionCtx::Qualified {
                parent: "mydb".into()
            }
        );
    }

    #[test]
    fn quotes_dont_tokenize_keywords() {
        // 'FROM' inside a string literal must not be classified as FROM.
        match parse("select 'from ' ") {
            CompletionCtx::Column { .. } => {}
            other => panic!("expected Column ctx, got {other:?}"),
        }
    }

    #[test]
    fn mid_string_offset() {
        let source = "select * from users; select ";
        let ctx = parse_context(source, 18); // inside `users`
        assert_eq!(ctx, CompletionCtx::Table);
    }
}
