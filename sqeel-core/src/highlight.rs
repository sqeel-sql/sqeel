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
    old_source: String,
}

impl Highlighter {
    pub fn new() -> anyhow::Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_sequel::LANGUAGE.into())?;
        Ok(Self {
            parser,
            old_tree: None,
            old_source: String::new(),
        })
    }

    pub fn highlight(&mut self, source: &str) -> Vec<HighlightSpan> {
        if source.is_empty() {
            self.old_tree = None;
            self.old_source.clear();
            return vec![];
        }

        // Apply edit info to old tree so tree-sitter can reuse unchanged nodes.
        if let Some(tree) = &mut self.old_tree
            && let Some(edit) = compute_input_edit(&self.old_source, source)
        {
            tree.edit(&edit);
        }

        let tree = match self.parser.parse(source, self.old_tree.as_ref()) {
            Some(t) => t,
            None => {
                self.old_tree = None;
                self.old_source.clear();
                return vec![];
            }
        };

        let source_bytes = source.as_bytes();
        let mut spans = Vec::new();
        collect_spans(tree.root_node(), source_bytes, &mut spans);

        self.old_source = source.to_owned();
        self.old_tree = Some(tree);

        spans
    }
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
