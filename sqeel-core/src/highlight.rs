use std::sync::Arc;
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    String,
    Comment,
    Number,
    Operator,
    Identifier,
    Plain,
}

/// SQL dialect the current connection is speaking. Drives per-dialect
/// keyword promotion in the highlighter so things like `ILIKE` show as
/// keywords on Postgres, `AUTO_INCREMENT` on MySQL, `PRAGMA` on SQLite,
/// etc. `Generic` means no dialect-specific extras — useful before any
/// connection has been established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Generic,
    MySql,
    Postgres,
    Sqlite,
}

impl Dialect {
    /// Pick a dialect from a sqlx-style URL scheme, matching the
    /// dispatch in `DbConnection::connect`.
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("mysql://") || url.starts_with("mariadb://") {
            Dialect::MySql
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Dialect::Postgres
        } else if url.starts_with("sqlite://") || url.starts_with("sqlite:") {
            Dialect::Sqlite
        } else {
            Dialect::Generic
        }
    }

    /// Extra identifiers that should render as keywords in this dialect,
    /// but aren't part of the shared tree-sitter-sequel keyword set.
    /// Compared case-insensitively against the token text.
    fn extra_keywords(self) -> &'static [&'static str] {
        match self {
            Dialect::MySql => &[
                "AUTO_INCREMENT",
                "ENGINE",
                "CHARSET",
                "COLLATE",
                "ZEROFILL",
                "UNSIGNED",
                "ROW_FORMAT",
                "KEY_BLOCK_SIZE",
                "DELAYED",
                "STRAIGHT_JOIN",
                "SQL_CALC_FOUND_ROWS",
                "LOW_PRIORITY",
                "HIGH_PRIORITY",
                "IGNORE",
            ],
            Dialect::Postgres => &[
                "ILIKE",
                "RETURNING",
                "SERIAL",
                "BIGSERIAL",
                "SMALLSERIAL",
                "BYTEA",
                "JSONB",
                "TSQUERY",
                "TSVECTOR",
                "GENERATED",
                "MATERIALIZED",
                "LATERAL",
                "DISTINCT",
                "CONCURRENTLY",
                "SIMILAR",
                "OVERLAPS",
            ],
            Dialect::Sqlite => &[
                "PRAGMA",
                "AUTOINCREMENT",
                "WITHOUT",
                "ROWID",
                "VACUUM",
                "GLOB",
                "ATTACH",
                "DETACH",
                "REINDEX",
                "SAVEPOINT",
            ],
            Dialect::Generic => &[],
        }
    }

    /// True iff `text` (any case) is one of this dialect's extra keywords
    /// OR a native-statement-start keyword. Both groups drive the same
    /// post-parse promotion so `DESC`, `SHOW`, `PRAGMA`, … render as
    /// keywords even though tree-sitter-sequel doesn't emit them as such.
    fn is_extra_keyword(self, text: &str) -> bool {
        self.extra_keywords()
            .iter()
            .chain(self.native_statement_starts().iter())
            .any(|kw| kw.eq_ignore_ascii_case(text))
    }

    /// Statement-start tokens that tree-sitter-sequel doesn't parse as
    /// valid statements but that the target engine accepts natively
    /// (e.g. MySQL's `DESC` / `SHOW`, SQLite's `PRAGMA`). When a
    /// statement begins with one of these, we skip the tree-sitter
    /// syntax gate and let the DB be the source of truth.
    fn native_statement_starts(self) -> &'static [&'static str] {
        match self {
            Dialect::MySql => &[
                "DESC",
                "DESCRIBE",
                "SHOW",
                "EXPLAIN",
                "USE",
                "ANALYZE",
                "OPTIMIZE",
                "REPAIR",
                "CHECK",
                "FLUSH",
                "KILL",
                "RENAME",
                "SET",
                "START",
                "COMMIT",
                "ROLLBACK",
                "SAVEPOINT",
                "LOAD",
                "GRANT",
                "REVOKE",
                "CALL",
            ],
            Dialect::Postgres => &[
                "EXPLAIN",
                "ANALYZE",
                "VACUUM",
                "CLUSTER",
                "COPY",
                "LISTEN",
                "NOTIFY",
                "UNLISTEN",
                "REINDEX",
                "REFRESH",
                "SET",
                "SHOW",
                "RESET",
                "BEGIN",
                "COMMIT",
                "ROLLBACK",
                "SAVEPOINT",
                "GRANT",
                "REVOKE",
                "CALL",
            ],
            Dialect::Sqlite => &[
                "PRAGMA",
                "VACUUM",
                "ATTACH",
                "DETACH",
                "REINDEX",
                "ANALYZE",
                "EXPLAIN",
                "BEGIN",
                "COMMIT",
                "ROLLBACK",
                "SAVEPOINT",
                "RELEASE",
            ],
            Dialect::Generic => &[],
        }
    }

    /// True iff `stmt`'s first non-comment token is one of this dialect's
    /// engine-native statement starts. Caller should skip the tree-sitter
    /// syntax gate for `true` and let the DB report any real error.
    pub fn is_native_statement(self, stmt: &str) -> bool {
        let stripped = strip_sql_comments(stmt);
        let trimmed = stripped.trim_start();
        let first_word: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
            .collect();
        if first_word.is_empty() {
            return false;
        }
        self.native_statement_starts()
            .iter()
            .any(|w| w.eq_ignore_ascii_case(&first_word))
    }
}

#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub kind: TokenKind,
}

pub struct Highlighter {
    parser: Parser,
    old_tree: Option<Tree>,
    // Held as `Arc<String>` so retaining a reference across highlight calls
    // is a ref-count bump instead of a full-buffer `String::clone` — which
    // on multi-MB buffers was the hottest allocation in the highlight loop.
    old_source: Option<Arc<String>>,
}

impl Highlighter {
    pub fn new() -> anyhow::Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_sequel::LANGUAGE.into())?;
        Ok(Self {
            parser,
            old_tree: None,
            old_source: None,
        })
    }

    /// Highlight a borrowed source string.  Callers that already have an
    /// `Arc<String>` should prefer [`Self::highlight_shared`] — this entry
    /// point has to allocate an `Arc<String>` copy to cache for the next
    /// diff.
    pub fn highlight(&mut self, source: &str, dialect: Dialect) -> Vec<HighlightSpan> {
        if source.is_empty() {
            self.old_tree = None;
            self.old_source = None;
            return vec![];
        }
        self.highlight_shared(&Arc::new(source.to_owned()), dialect)
    }

    /// Highlight a shared source buffer.  The `Arc<String>` is retained
    /// (ref-count bumped) for use as the incremental-edit diff base on
    /// the next call — avoids the multi-MB `String::clone` the old design
    /// paid on every highlight of a huge file.
    pub fn highlight_shared(
        &mut self,
        source: &Arc<String>,
        dialect: Dialect,
    ) -> Vec<HighlightSpan> {
        if source.is_empty() {
            self.old_tree = None;
            self.old_source = None;
            return vec![];
        }

        // Apply edit info to old tree so tree-sitter can reuse unchanged nodes.
        if let Some(tree) = &mut self.old_tree
            && let Some(old) = self.old_source.as_deref()
            && let Some(edit) = compute_input_edit(old, source)
        {
            tree.edit(&edit);
        }

        let tree = match self.parser.parse(source.as_str(), self.old_tree.as_ref()) {
            Some(t) => t,
            None => {
                self.old_tree = None;
                self.old_source = None;
                return vec![];
            }
        };

        let source_bytes = source.as_bytes();
        let mut spans = Vec::new();
        collect_spans(tree.root_node(), source_bytes, dialect, &mut spans);
        // tree-sitter-sequel can drop whole regions on parse recovery
        // (e.g. `DESC users;` sitting after a valid SELECT emits no
        // child spans at all). Sweep any byte-range not covered by a
        // tree-sitter span for dialect-specific keywords and emit
        // synthetic Keyword spans for those.
        promote_uncovered_dialect_keywords(source, dialect, &mut spans);

        self.old_source = Some(Arc::clone(source));
        self.old_tree = Some(tree);

        spans
    }
}

/// Find identifier-shaped words in regions that tree-sitter didn't
/// classify, and emit Keyword spans for those that match the active
/// dialect's extra-keyword or native-statement list.
fn promote_uncovered_dialect_keywords(
    source: &str,
    dialect: Dialect,
    spans: &mut Vec<HighlightSpan>,
) {
    if matches!(dialect, Dialect::Generic) {
        return;
    }
    // Fast reject: if tree-sitter covered every byte we're done.
    let total = source.len();
    if total == 0 {
        return;
    }
    // Sort a shallow copy of existing span ranges for binary-search
    // style sweep. We only need to know which bytes are covered.
    let mut covered: Vec<(usize, usize)> =
        spans.iter().map(|s| (s.start_byte, s.end_byte)).collect();
    covered.sort_by_key(|&(s, _)| s);

    // Merge overlapping/adjacent ranges for a clean "uncovered" sweep.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(covered.len());
    for (s, e) in covered {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
        } else {
            merged.push((s, e));
        }
    }

    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut gap_iter = merged.iter().peekable();
    let mut additions: Vec<HighlightSpan> = Vec::new();
    while cursor < total {
        match gap_iter.peek().copied() {
            Some(&(gs, ge)) if gs <= cursor => {
                cursor = ge;
                gap_iter.next();
            }
            Some(&(gs, _)) => {
                scan_gap_for_keywords(source, bytes, cursor, gs, dialect, &mut additions);
                cursor = gs;
            }
            None => {
                scan_gap_for_keywords(source, bytes, cursor, total, dialect, &mut additions);
                cursor = total;
            }
        }
    }
    spans.extend(additions);
    spans.sort_by_key(|s| s.start_byte);
}

fn scan_gap_for_keywords(
    source: &str,
    bytes: &[u8],
    start: usize,
    end: usize,
    dialect: Dialect,
    out: &mut Vec<HighlightSpan>,
) {
    let mut i = start;
    while i < end {
        let b = bytes[i];
        if !(b.is_ascii_alphabetic() || b == b'_') {
            i += 1;
            continue;
        }
        let word_start = i;
        while i < end {
            let c = bytes[i];
            if !(c.is_ascii_alphanumeric() || c == b'_') {
                break;
            }
            i += 1;
        }
        let word = &source[word_start..i];
        if dialect.is_extra_keyword(word) {
            let (sr, sc) = byte_to_point_rowcol(source, word_start);
            let (er, ec) = byte_to_point_rowcol(source, i);
            out.push(HighlightSpan {
                start_byte: word_start,
                end_byte: i,
                start_row: sr,
                start_col: sc,
                end_row: er,
                end_col: ec,
                kind: TokenKind::Keyword,
            });
        }
    }
}

fn byte_to_point_rowcol(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source[..byte.min(source.len())];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let col = prefix.bytes().rev().take_while(|&b| b != b'\n').count();
    (row, col)
}

/// Parse `source` and return the byte ranges of each top-level statement.
/// Whitespace between statements is excluded. Statements are returned in source order.
pub fn statement_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .is_err()
    {
        return vec![];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };
    let root = tree.root_node();
    let mut ranges = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let start = child.start_byte();
        let end = child.end_byte();
        if start < end && end <= source.len() {
            ranges.push((start, end));
        }
    }
    if ranges.is_empty() && !source.trim().is_empty() {
        ranges.push((0, source.len()));
    }
    // Filter out ranges that are just semicolons or whitespace (tree-sitter-sequel
    // creates separate anonymous nodes for `;` delimiters between statements).
    ranges.retain(|&(s, e)| !source[s..e].trim().is_empty() && source[s..e].trim() != ";");
    ranges
}

/// Returns the byte range of the statement containing `byte`. If `byte` falls in
/// inter-statement whitespace, returns the statement immediately preceding it
/// (or the next one if it precedes the first statement).
pub fn statement_at_byte(source: &str, byte: usize) -> Option<(usize, usize)> {
    let ranges = statement_ranges(source);
    if ranges.is_empty() {
        return None;
    }
    for (i, (s, e)) in ranges.iter().enumerate() {
        if byte >= *s && byte < *e {
            return Some((*s, *e));
        }
        if byte < *s {
            return Some(if i == 0 { ranges[0] } else { ranges[i - 1] });
        }
    }
    Some(*ranges.last().unwrap())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub line: usize,
    pub col: usize,
    pub byte: usize,
    pub message: String,
}

/// Parse `source` and return the first syntax error (line/col 1-based) with a
/// human-readable message, or `None` if the SQL parses cleanly.
pub fn first_syntax_error(source: &str) -> Option<SyntaxError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if !root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_missing() {
            let p = node.start_position();
            let kind = node.kind();
            let message = if kind.is_empty() {
                "missing token".to_string()
            } else {
                format!("missing `{kind}`")
            };
            return Some(SyntaxError {
                line: p.row + 1,
                col: p.column + 1,
                byte: node.start_byte(),
                message,
            });
        }
        if node.is_error() {
            let p = node.start_position();
            let snippet = source
                .get(node.start_byte()..node.end_byte())
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .trim();
            let message = if snippet.is_empty() {
                "unexpected token".to_string()
            } else {
                let trimmed: String = snippet.chars().take(40).collect();
                format!("unexpected `{trimmed}`")
            };
            return Some(SyntaxError {
                line: p.row + 1,
                col: p.column + 1,
                byte: node.start_byte(),
                message,
            });
        }
        for child in node.children(&mut cursor) {
            if child.has_error() || child.is_error() || child.is_missing() {
                stack.push(child);
            }
        }
    }
    Some(SyntaxError {
        line: 1,
        col: 1,
        byte: 0,
        message: "parse error".to_string(),
    })
}

/// Strip SQL comments (`-- …` line comments and `/* … */` block comments) from
/// `source`, preserving comment-like content inside single-quoted, double-quoted,
/// and backtick-quoted strings. Block comments collapse to a single space so
/// adjacent tokens stay separated.
pub fn strip_sql_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' | b'`' => {
                out.push(c);
                i += 1;
                while i < bytes.len() {
                    let d = bytes[i];
                    out.push(d);
                    i += 1;
                    if d == c {
                        break;
                    }
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    i += 2;
                }
                out.push(b' ');
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

/// True when `query` is a `SHOW CREATE ...` statement. Leading whitespace and
/// SQL comments are skipped before matching.
pub fn is_show_create(query: &str) -> bool {
    let stripped = strip_sql_comments(query);
    let trimmed = stripped.trim_start();
    trimmed.len() >= 11 && trimmed[..11].eq_ignore_ascii_case("show create")
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new().expect("failed to initialize tree-sitter-sequel")
    }
}

/// Computes the minimal `InputEdit` needed to inform tree-sitter of what changed
/// between `old` and `new`. Returns `None` if the strings are identical.
fn compute_input_edit(old: &str, new: &str) -> Option<InputEdit> {
    let old_b = old.as_bytes();
    let new_b = new.as_bytes();

    // Scan forward to find the first differing byte.
    let start_byte = old_b
        .iter()
        .zip(new_b.iter())
        .take_while(|(a, b)| a == b)
        .count();

    if start_byte == old_b.len() && start_byte == new_b.len() {
        return None; // identical
    }

    // Scan backward to find the last differing byte (common suffix length).
    let max_suffix = (old_b.len() - start_byte).min(new_b.len() - start_byte);
    let common_suffix = old_b[start_byte..]
        .iter()
        .rev()
        .zip(new_b[start_byte..].iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();

    let old_end_byte = old_b.len() - common_suffix;
    let new_end_byte = new_b.len() - common_suffix;

    Some(InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position: byte_to_point(old, start_byte),
        old_end_position: byte_to_point(old, old_end_byte),
        new_end_position: byte_to_point(new, new_end_byte),
    })
}

fn byte_to_point(s: &str, byte_offset: usize) -> Point {
    let prefix = &s[..byte_offset.min(s.len())];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let col = prefix.bytes().rev().take_while(|&b| b != b'\n').count();
    Point { row, column: col }
}

fn named_node_kind(kind: &str) -> TokenKind {
    match kind {
        k if k.contains("keyword") => TokenKind::Keyword,
        k if k.contains("string") || k.contains("literal") || k == "quoted_identifier" => {
            TokenKind::String
        }
        k if k.contains("comment") => TokenKind::Comment,
        k if k.contains("number") || k.contains("integer") || k.contains("float") => {
            TokenKind::Number
        }
        k if k.contains("operator") => TokenKind::Operator,
        k if k.contains("identifier") || k.contains("name") => TokenKind::Identifier,
        _ => TokenKind::Plain,
    }
}

fn anon_node_kind(text: &str) -> TokenKind {
    match text {
        "=" | "!=" | "<>" | "<" | ">" | "<=" | ">=" | "+" | "-" | "*" | "/" | "%" => {
            TokenKind::Operator
        }
        _ => TokenKind::Plain,
    }
}

fn collect_spans(node: Node, source: &[u8], dialect: Dialect, spans: &mut Vec<HighlightSpan>) {
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
    if start_byte >= end_byte || end_byte > source.len() {
        return;
    }

    let text = std::str::from_utf8(&source[start_byte..end_byte]).unwrap_or("");
    let start = node.start_position();
    let end = node.end_position();

    if node.child_count() == 0 && node.is_named() {
        let mut kind = named_node_kind(node.kind());
        // Dialect post-promotion: tree-sitter-sequel's keyword set is
        // the union across SQL dialects but misses some MySQL / Postgres
        // / SQLite specials. Bump identifiers / plains up to keyword
        // when they match the active dialect's extra-keyword list.
        if matches!(kind, TokenKind::Identifier | TokenKind::Plain)
            && dialect.is_extra_keyword(text)
        {
            kind = TokenKind::Keyword;
        }
        if kind != TokenKind::Plain {
            spans.push(HighlightSpan {
                start_byte,
                end_byte,
                start_row: start.row,
                start_col: start.column,
                end_row: end.row,
                end_col: end.column,
                kind,
            });
            return;
        }
    }

    if node.child_count() == 0 {
        let mut kind = anon_node_kind(text);
        if kind == TokenKind::Plain && dialect.is_extra_keyword(text) {
            kind = TokenKind::Keyword;
        }
        if kind != TokenKind::Plain {
            spans.push(HighlightSpan {
                start_byte,
                end_byte,
                start_row: start.row,
                start_col: start.column,
                end_row: end.row,
                end_col: end.column,
                kind,
            });
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_spans(child, source, dialect, spans);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_statement_skips_semicolon_nodes() {
        let src = "select * from foo;\nselect * from bar;";
        let ranges = statement_ranges(src);
        for (s, e) in &ranges {
            let stmt = &src[*s..*e];
            assert!(
                stmt.trim() != ";",
                "statement_ranges should not return semicolon-only ranges"
            );
            assert!(
                !stmt.trim().is_empty(),
                "statement_ranges should not return empty ranges"
            );
        }
        // Each statement should parse cleanly
        for (s, e) in &ranges {
            let stmt = &src[*s..*e].trim();
            let err = first_syntax_error(stmt);
            assert!(
                err.is_none(),
                "expected no syntax error for {:?}, got: {:?}",
                stmt,
                err
            );
        }
    }

    #[test]
    fn highlights_select_keyword() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT id FROM users", Dialect::Generic);
        let keywords: Vec<_> = spans
            .iter()
            .filter(|s| s.kind == TokenKind::Keyword)
            .collect();
        assert!(
            !keywords.is_empty(),
            "expected keyword spans, got: {spans:#?}"
        );
    }

    #[test]
    fn highlights_identifier() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT id FROM users", Dialect::Generic);
        let idents: Vec<_> = spans
            .iter()
            .filter(|s| s.kind == TokenKind::Identifier)
            .collect();
        assert!(
            !idents.is_empty(),
            "expected identifier spans, got: {spans:#?}"
        );
    }

    #[test]
    fn highlights_string_literal() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT * FROM users WHERE name = 'alice'", Dialect::Generic);
        let strings: Vec<_> = spans
            .iter()
            .filter(|s| s.kind == TokenKind::String)
            .collect();
        assert!(
            !strings.is_empty(),
            "expected string spans, got: {spans:#?}"
        );
    }

    #[test]
    fn empty_input_no_panic() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("", Dialect::Generic);
        assert!(spans.is_empty());
    }

    #[test]
    fn invalid_sql_no_panic() {
        let mut h = Highlighter::new().unwrap();
        let _spans = h.highlight("??? !!! garbage", Dialect::Generic);
    }

    #[test]
    fn incremental_same_result() {
        let mut h = Highlighter::new().unwrap();
        let src1 = "SELECT id FROM users";
        let src2 = "SELECT id FROM users WHERE id = 1";
        let spans_full = {
            let mut h2 = Highlighter::new().unwrap();
            h2.highlight(src2, Dialect::Generic)
        };
        h.highlight(src1, Dialect::Generic);
        let spans_incr = h.highlight(src2, Dialect::Generic);
        assert_eq!(spans_full.len(), spans_incr.len());
    }

    #[test]
    fn dialect_from_url_dispatch() {
        assert_eq!(Dialect::from_url("mysql://u:p@h/d"), Dialect::MySql);
        assert_eq!(Dialect::from_url("mariadb://u:p@h/d"), Dialect::MySql);
        assert_eq!(Dialect::from_url("postgres://h/d"), Dialect::Postgres);
        assert_eq!(Dialect::from_url("postgresql://h/d"), Dialect::Postgres);
        assert_eq!(Dialect::from_url("sqlite:///tmp/a.db"), Dialect::Sqlite);
        assert_eq!(Dialect::from_url("sqlite:a.db"), Dialect::Sqlite);
        assert_eq!(Dialect::from_url("other://x"), Dialect::Generic);
    }

    #[test]
    fn mysql_auto_increment_promoted_to_keyword() {
        let src = "CREATE TABLE t (id INT AUTO_INCREMENT)";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let has = spans.iter().any(|s| {
            s.kind == TokenKind::Keyword && &src[s.start_byte..s.end_byte] == "AUTO_INCREMENT"
        });
        assert!(has, "AUTO_INCREMENT should be a keyword on MySQL");
    }

    #[test]
    fn dialect_extra_keyword_tables_are_non_empty() {
        // The `Generic` dialect has no extras; every concrete dialect
        // must carry at least one dialect-specific keyword so the
        // post-pass actually does something.
        assert!(!Dialect::MySql.extra_keywords().is_empty());
        assert!(!Dialect::Postgres.extra_keywords().is_empty());
        assert!(!Dialect::Sqlite.extra_keywords().is_empty());
        assert!(Dialect::Generic.extra_keywords().is_empty());
    }

    #[test]
    fn is_native_statement_matches_leading_token() {
        assert!(Dialect::MySql.is_native_statement("DESC users"));
        assert!(Dialect::MySql.is_native_statement("desc users"));
        assert!(Dialect::MySql.is_native_statement("DESCRIBE users"));
        assert!(Dialect::MySql.is_native_statement("SHOW TABLES"));
        assert!(Dialect::MySql.is_native_statement("-- lead\nDESC users"));
        assert!(!Dialect::MySql.is_native_statement("SELECT * FROM users"));

        assert!(Dialect::Sqlite.is_native_statement("PRAGMA foreign_keys = ON"));
        assert!(!Dialect::Sqlite.is_native_statement("DESC users")); // DESC is MySQL-only here
    }

    #[test]
    fn is_native_statement_skips_leading_comments_and_whitespace() {
        assert!(Dialect::MySql.is_native_statement("   \n  -- comment line\n  DESC users;\n"));
    }

    #[test]
    fn is_extra_keyword_is_case_insensitive() {
        assert!(Dialect::MySql.is_extra_keyword("auto_increment"));
        assert!(Dialect::MySql.is_extra_keyword("AUTO_INCREMENT"));
        assert!(Dialect::Postgres.is_extra_keyword("ilike"));
        assert!(!Dialect::MySql.is_extra_keyword("ilike"));
    }

    #[test]
    fn desc_lowercase_select_prior_is_keyword() {
        // Reproduces the exact screenshot shape: lowercase select then
        // two blocks of `DESC users;` separated by blank lines.
        let src = "select * from users;\n\nDESC users;\n\nDESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let desc_kw_count = spans
            .iter()
            .filter(|s| s.kind == TokenKind::Keyword && &src[s.start_byte..s.end_byte] == "DESC")
            .count();
        assert_eq!(
            desc_kw_count, 2,
            "expected both DESCs highlighted; spans: {spans:#?}"
        );
    }

    #[test]
    fn generic_dialect_skips_desc_keyword_promotion() {
        // Regression: if the TUI plumbs `Generic` to the highlight
        // worker (e.g. because `active_dialect` wasn't updated after
        // connect), our dialect-specific sweep no-ops and DESC stays
        // unhighlighted. Confirms the dialect actually drives the
        // promotion so a dialect-propagation regression flips the count.
        let src = "select * from users;\n\nDESC users;\n\nDESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let generic = h
            .highlight(src, Dialect::Generic)
            .into_iter()
            .filter(|s| s.kind == TokenKind::Keyword && &src[s.start_byte..s.end_byte] == "DESC")
            .count();
        let mut h2 = Highlighter::new().unwrap();
        let mysql = h2
            .highlight(src, Dialect::MySql)
            .into_iter()
            .filter(|s| s.kind == TokenKind::Keyword && &src[s.start_byte..s.end_byte] == "DESC")
            .count();
        assert!(
            mysql > generic,
            "MySql must promote more DESCs to keyword than Generic (mysql={mysql}, generic={generic})"
        );
        assert_eq!(mysql, 2, "expected both DESCs highlighted under MySql");
    }

    #[test]
    fn debug_dump_with_alter_tail() {
        // Match user's actual buffer: the header lines + 40 repeated
        // `-- ALTER TABLE …` lines at the bottom, totalling ~64 rows.
        let header = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n\
                   \n";
        let alter = "-- ALTER TABLE ppc_third.`searches_182` ADD COLUMN `error` TEXT NULL AFTER `status`;\n";
        let mut src = header.to_string();
        for _ in 0..40 {
            src.push_str(alter);
        }

        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(&src, Dialect::MySql);
        for s in &spans {
            let t = &src[s.start_byte..s.end_byte];
            let sr = s.start_row;
            if (19..=25).contains(&sr) {
                println!(
                    "{:?} r{}:{}-{}:{} byte={}..{} text={:?}",
                    s.kind, sr, s.start_col, s.end_row, s.end_col, s.start_byte, s.end_byte, t
                );
            }
        }
        let desc_count = spans
            .iter()
            .filter(|s| s.kind == TokenKind::Keyword && &src[s.start_byte..s.end_byte] == "DESC")
            .count();
        println!("DESC keyword count = {}", desc_count);
    }

    #[test]
    fn debug_dump_full_buffer_spans() {
        let src = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        for s in &spans {
            let t = &src[s.start_byte..s.end_byte];
            let sr = s.start_row;
            let er = s.end_row;
            if (19..=25).contains(&sr) || (19..=25).contains(&er) {
                println!(
                    "{:?} r{}:{}-{}:{} byte={}..{} text={:?}",
                    s.kind, sr, s.start_col, er, s.end_col, s.start_byte, s.end_byte, t
                );
            }
        }
    }

    #[test]
    fn desc_highlighted_in_full_buffer_repro() {
        let src = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);

        // Both DESC statement-starts must render as keywords.
        let desc_kw_positions: Vec<usize> = spans
            .iter()
            .filter(|s| s.kind == TokenKind::Keyword && &src[s.start_byte..s.end_byte] == "DESC")
            .map(|s| s.start_byte)
            .collect();
        let expected = src
            .match_indices("DESC ")
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert_eq!(
            desc_kw_positions, expected,
            "all DESC instances should be keyword spans; got positions {desc_kw_positions:?} for expected {expected:?}; spans: {spans:#?}"
        );
    }

    #[test]
    fn desc_highlighted_after_repeated_incremental_edits() {
        // The live HighlightThread reuses one Highlighter across many
        // edits with incremental re-parses. Drive a burst of edits and
        // confirm DESC stays a Keyword span each time.
        let mut h = Highlighter::new().unwrap();
        let seeds = [
            "select * from users;\n",
            "select * from users;\nD",
            "select * from users;\nDE",
            "select * from users;\nDESC",
            "select * from users;\nDESC ",
            "select * from users;\nDESC users;\n",
            "select * from users;\n\nDESC users;\n",
            "select * from users;\n\nDESC users;\n\nDESC users;\n",
        ];
        for src in seeds {
            h.highlight(src, Dialect::MySql);
        }
        let final_src = "select * from users;\n\nDESC users;\n\nDESC users;\n";
        let spans = h.highlight(final_src, Dialect::MySql);
        let count = spans
            .iter()
            .filter(|s| {
                s.kind == TokenKind::Keyword && &final_src[s.start_byte..s.end_byte] == "DESC"
            })
            .count();
        assert_eq!(count, 2, "expected 2 DESC keyword spans; got: {spans:#?}");
    }

    #[test]
    fn desc_survives_incremental_edit() {
        // The live highlight-thread retains the tree across edits and
        // re-parses incrementally. Seed with a plain SELECT, then edit
        // the source to append a DESC line and re-parse — the DESC must
        // still pick up a keyword span.
        let mut h = Highlighter::new().unwrap();
        let seed = "SELECT * FROM users;\n";
        h.highlight(seed, Dialect::MySql);

        let edited = "SELECT * FROM users;\nDESC users;\n";
        let spans = h.highlight(edited, Dialect::MySql);
        let has_desc_kw = spans
            .iter()
            .any(|s| s.kind == TokenKind::Keyword && &edited[s.start_byte..s.end_byte] == "DESC");
        assert!(
            has_desc_kw,
            "DESC should be a keyword after incremental parse; spans: {spans:#?}"
        );
    }

    #[test]
    fn desc_after_prior_statement_is_keyword() {
        // Regression: `DESC users;` sitting after a valid SELECT was
        // rendering unhighlighted in the TUI while an identical line
        // elsewhere rendered as a keyword. Likely tree-sitter-sequel
        // emits the whole error span as one anonymous leaf when the
        // prior statement nudges recovery — the post-pass only checked
        // exact single-token text, so "DESC users;" failed the match.
        let src = "SELECT * FROM users;\nDESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let has_desc_kw = spans
            .iter()
            .any(|s| s.kind == TokenKind::Keyword && &src[s.start_byte..s.end_byte] == "DESC");
        assert!(
            has_desc_kw,
            "expected DESC to be a keyword span; spans: {spans:#?}"
        );
    }

    #[test]
    fn native_statement_starts_also_promote_to_keyword() {
        // DESC / SHOW / PRAGMA aren't in `extra_keywords` but still need
        // keyword styling — covered via the chained native-start list.
        assert!(Dialect::MySql.is_extra_keyword("DESC"));
        assert!(Dialect::MySql.is_extra_keyword("SHOW"));
        assert!(Dialect::Sqlite.is_extra_keyword("PRAGMA"));
        assert!(Dialect::Postgres.is_extra_keyword("LISTEN"));
        assert!(!Dialect::Postgres.is_extra_keyword("DESC")); // MySQL-only
    }

    #[test]
    fn compute_edit_single_insert() {
        // "SELECT id FROM users" → "SELECT idx FROM users"
        // First diff at byte 9: old=' ', new='x'. Nothing deleted, one byte inserted.
        let old = "SELECT id FROM users";
        let new = "SELECT idx FROM users";
        let edit = compute_input_edit(old, new).unwrap();
        assert_eq!(edit.start_byte, 9);
        assert_eq!(edit.old_end_byte, 9);
        assert_eq!(edit.new_end_byte, 10);
    }
}
