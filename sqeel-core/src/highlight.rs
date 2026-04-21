use tree_sitter::{Node, Parser};

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
}

const SQL_KEYWORDS: &[&str] = &[
    "select", "from", "where", "insert", "into", "values", "update", "set",
    "delete", "create", "table", "drop", "alter", "add", "column", "join",
    "inner", "outer", "left", "right", "full", "cross", "on", "and", "or",
    "not", "null", "is", "in", "like", "between", "order", "by", "group",
    "having", "limit", "offset", "union", "all", "distinct", "as", "case",
    "when", "then", "else", "end", "if", "exists", "primary", "foreign",
    "key", "references", "unique", "default", "constraint", "check", "with",
    "view", "begin", "commit", "rollback", "transaction", "use", "show",
    "describe", "explain", "database", "schema", "index", "procedure",
    "function", "returns", "return", "trigger", "true", "false",
];

impl Highlighter {
    pub fn new() -> anyhow::Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_sequel::LANGUAGE.into())?;
        Ok(Self { parser })
    }

    pub fn highlight(&mut self, source: &str) -> Vec<HighlightSpan> {
        if source.is_empty() {
            return vec![];
        }
        let tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return vec![],
        };
        let source_bytes = source.as_bytes();
        let mut spans = Vec::new();
        collect_spans(tree.root_node(), source_bytes, &mut spans);
        spans
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new().expect("failed to initialize tree-sitter-sequel")
    }
}

fn named_node_kind(kind: &str) -> TokenKind {
    match kind {
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
    let lower = text.to_ascii_lowercase();
    if SQL_KEYWORDS.contains(&lower.as_str()) {
        return TokenKind::Keyword;
    }
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

    // Named leaf nodes: check kind for strings, comments, numbers, identifiers
    if node.child_count() == 0 && node.is_named() {
        let kind = named_node_kind(node.kind());
        if kind != TokenKind::Plain {
            spans.push(HighlightSpan {
                start_byte, end_byte,
                start_row: start.row, start_col: start.column,
                end_row: end.row, end_col: end.column,
                kind,
            });
            return;
        }
    }

    // Any leaf node (named or anon): check if text is a SQL keyword or operator
    if node.child_count() == 0 {
        let kind = anon_node_kind(text);
        if kind != TokenKind::Plain {
            spans.push(HighlightSpan {
                start_byte, end_byte,
                start_row: start.row, start_col: start.column,
                end_row: end.row, end_col: end.column,
                kind,
            });
        }
        return;
    }

    // Check if this named node wraps a single-token keyword (no whitespace children)
    // Some grammars wrap keywords in named nodes with one child
    if node.is_named() && node.child_count() == 1 {
        let kind = named_node_kind(node.kind());
        if kind == TokenKind::Keyword {
            spans.push(HighlightSpan {
                start_byte, end_byte,
                start_row: start.row, start_col: start.column,
                end_row: end.row, end_col: end.column,
                kind,
            });
            return;
        }
        // Also check if the text itself is a keyword
        let text_kind = anon_node_kind(text);
        if text_kind == TokenKind::Keyword {
            spans.push(HighlightSpan {
                start_byte, end_byte,
                start_row: start.row, start_col: start.column,
                end_row: end.row, end_col: end.column,
                kind: TokenKind::Keyword,
            });
            return;
        }
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
    fn highlights_select_keyword() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT id FROM users");
        let keywords: Vec<_> = spans.iter().filter(|s| s.kind == TokenKind::Keyword).collect();
        assert!(!keywords.is_empty(), "expected keyword spans, got: {spans:#?}");
    }

    #[test]
    fn highlights_identifier() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT id FROM users");
        let idents: Vec<_> = spans.iter().filter(|s| s.kind == TokenKind::Identifier).collect();
        assert!(!idents.is_empty(), "expected identifier spans, got: {spans:#?}");
    }

    #[test]
    fn highlights_string_literal() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT * FROM users WHERE name = 'alice'");
        let strings: Vec<_> = spans.iter().filter(|s| s.kind == TokenKind::String).collect();
        assert!(!strings.is_empty(), "expected string spans, got: {spans:#?}");
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
}
