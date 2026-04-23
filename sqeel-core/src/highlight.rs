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
    pub fn highlight(&mut self, source: &str) -> Vec<HighlightSpan> {
        if source.is_empty() {
            self.old_tree = None;
            self.old_source = None;
            return vec![];
        }
        self.highlight_shared(&Arc::new(source.to_owned()))
    }

    /// Highlight a shared source buffer.  The `Arc<String>` is retained
    /// (ref-count bumped) for use as the incremental-edit diff base on
    /// the next call — avoids the multi-MB `String::clone` the old design
    /// paid on every highlight of a huge file.
    pub fn highlight_shared(&mut self, source: &Arc<String>) -> Vec<HighlightSpan> {
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
        collect_spans(tree.root_node(), source_bytes, &mut spans);

        self.old_source = Some(Arc::clone(source));
        self.old_tree = Some(tree);

        spans
    }
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

fn collect_spans(node: Node, source: &[u8], spans: &mut Vec<HighlightSpan>) {
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
    if start_byte >= end_byte || end_byte > source.len() {
        return;
    }

    let text = std::str::from_utf8(&source[start_byte..end_byte]).unwrap_or("");
    let start = node.start_position();
    let end = node.end_position();

    if node.child_count() == 0 && node.is_named() {
        let kind = named_node_kind(node.kind());
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
        let kind = anon_node_kind(text);
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
        collect_spans(child, source, spans);
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
        let spans = h.highlight("SELECT id FROM users");
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
        let spans = h.highlight("SELECT id FROM users");
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
        let spans = h.highlight("SELECT * FROM users WHERE name = 'alice'");
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
        let spans = h.highlight("");
        assert!(spans.is_empty());
    }

    #[test]
    fn invalid_sql_no_panic() {
        let mut h = Highlighter::new().unwrap();
        let _spans = h.highlight("??? !!! garbage");
    }

    #[test]
    fn incremental_same_result() {
        let mut h = Highlighter::new().unwrap();
        let src1 = "SELECT id FROM users";
        let src2 = "SELECT id FROM users WHERE id = 1";
        let spans_full = {
            let mut h2 = Highlighter::new().unwrap();
            h2.highlight(src2)
        };
        h.highlight(src1);
        let spans_incr = h.highlight(src2);
        assert_eq!(spans_full.len(), spans_incr.len());
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
