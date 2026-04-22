//! Ex-command parser + executor.
//!
//! Parses the text after a leading `:` in the command-line prompt and
//! returns an [`ExEffect`] describing what the caller should do. Only the
//! editor-local effects (substitute, goto-line, clear-highlight) are
//! applied in-place against `Editor`; quit / save / unknown are returned
//! to the caller so the TUI loop can run them.

use super::Editor;
use tui_textarea::CursorMove;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExEffect {
    /// Nothing happened (empty input or already-applied effect).
    None,
    /// Save the current buffer.
    Save,
    /// Quit (`:q`, `:q!`, `:wq`, `:x`).
    Quit { force: bool, save: bool },
    /// Unknown command — caller should surface as an error toast.
    Unknown(String),
    /// Substitution finished — report replacement count.
    Substituted { count: usize },
    /// A no-op response for successful commands that don't need a side
    /// effect but should not be reported as unknown (e.g. `:noh`).
    Ok,
    /// Surface an informational message.
    Info(String),
    /// Surface an error message (syntax error, bad pattern, …).
    Error(String),
}

/// Parse and execute `input` (without the leading `:`).
pub fn run(editor: &mut Editor<'_>, input: &str) -> ExEffect {
    let cmd = input.trim();
    if cmd.is_empty() {
        return ExEffect::None;
    }

    // Bare line number — jump there.
    if let Ok(line) = cmd.parse::<usize>() {
        editor.goto_line(line);
        return ExEffect::Ok;
    }

    // `:q`, `:q!`, `:w`, `:wq`, `:x`.
    match cmd {
        "q" => {
            return ExEffect::Quit {
                force: false,
                save: false,
            };
        }
        "q!" => {
            return ExEffect::Quit {
                force: true,
                save: false,
            };
        }
        "w" => return ExEffect::Save,
        "wq" | "x" => {
            return ExEffect::Quit {
                force: false,
                save: true,
            };
        }
        "noh" | "nohlsearch" => {
            // Clearing the pattern removes the highlight.
            let _ = editor.textarea.set_search_pattern("");
            return ExEffect::Ok;
        }
        _ => {}
    }

    // `:s/...` or `:%s/...` substitute.
    let (scope, rest) = if let Some(rest) = cmd.strip_prefix("%s") {
        (SubScope::Whole, rest)
    } else if let Some(rest) = cmd.strip_prefix('s') {
        (SubScope::CurrentLine, rest)
    } else {
        return ExEffect::Unknown(cmd.to_string());
    };
    match parse_substitute_body(rest) {
        Ok(sub) => match apply_substitute(editor, scope, sub) {
            Ok(count) => ExEffect::Substituted { count },
            Err(e) => ExEffect::Error(e),
        },
        Err(e) => ExEffect::Error(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubScope {
    CurrentLine,
    Whole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Substitute {
    pattern: String,
    replacement: String,
    global: bool,
    case_insensitive: bool,
}

/// Parse the `/pat/repl/flags` tail of a substitute command. The leading
/// `s` or `%s` has already been stripped. The separator is the first
/// character after the optional scope (typically `/`), matching vim.
fn parse_substitute_body(body: &str) -> Result<Substitute, String> {
    let mut chars = body.chars();
    let sep = chars.next().ok_or_else(|| "empty substitute".to_string())?;
    if sep.is_alphanumeric() || sep == '\\' {
        return Err("substitute needs a separator, e.g. :s/foo/bar/".into());
    }
    let rest: String = chars.collect();
    let parts = split_unescaped(&rest, sep);
    if parts.len() < 2 {
        return Err("substitute needs /pattern/replacement/".into());
    }
    let pattern = unescape(&parts[0], sep);
    let replacement = unescape(&parts[1], sep);
    let flags = parts.get(2).cloned().unwrap_or_default();
    let mut global = false;
    let mut case_insensitive = false;
    for f in flags.chars() {
        match f {
            'g' => global = true,
            'i' => case_insensitive = true,
            'c' => {
                return Err("interactive substitution (c flag) is not supported".into());
            }
            other => return Err(format!("unknown substitute flag: {other}")),
        }
    }
    Ok(Substitute {
        pattern,
        replacement,
        global,
        case_insensitive,
    })
}

/// Split `s` by `sep`, treating `\<sep>` as a literal occurrence.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                // Preserve the escape so regex metachars survive; only
                // collapse an escaped separator into a literal separator.
                if next == sep {
                    cur.push(sep);
                    chars.next();
                } else {
                    cur.push('\\');
                    cur.push(next);
                    chars.next();
                }
            } else {
                cur.push('\\');
            }
        } else if c == sep {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

/// Remove our `\<sep>` → `<sep>` escape. Other `\x` sequences pass
/// through so regex escape syntax (`\b`, `\d`, …) still works.
fn unescape(s: &str, _sep: char) -> String {
    s.to_string()
}

fn apply_substitute(
    editor: &mut Editor<'_>,
    scope: SubScope,
    sub: Substitute,
) -> Result<usize, String> {
    let pattern = if sub.case_insensitive {
        format!("(?i){}", sub.pattern)
    } else {
        sub.pattern.clone()
    };
    let regex = regex::Regex::new(&pattern).map_err(|e| format!("bad pattern: {e}"))?;

    editor.push_undo();

    let (range_start, range_end) = match scope {
        SubScope::CurrentLine => {
            let r = editor.textarea.cursor().0;
            (r, r)
        }
        SubScope::Whole => (0, editor.textarea.lines().len().saturating_sub(1)),
    };

    let mut new_lines: Vec<String> = editor.textarea.lines().to_vec();
    let mut count = 0usize;
    let clamp = range_end.min(new_lines.len().saturating_sub(1));
    for line in new_lines[range_start..=clamp].iter_mut() {
        let (replaced, n) = regex_replace(&regex, line, &sub.replacement, sub.global);
        *line = replaced;
        count += n;
    }

    if count == 0 {
        // Undo the undo push so the user doesn't see an empty undo step.
        editor.undo_stack.pop();
        return Ok(0);
    }

    // Apply the new content without clobbering the yank buffer / session state.
    let carried_yank = editor.textarea.yank_text();
    editor.textarea = tui_textarea::TextArea::new(new_lines);
    editor.textarea.set_max_histories(0);
    if !carried_yank.is_empty() {
        editor.textarea.set_yank_text(carried_yank);
    }
    editor
        .textarea
        .move_cursor(CursorMove::Jump(range_start, 0));
    editor.mark_dirty_after_ex();
    Ok(count)
}

/// Count-returning variant of `Regex::replace` / `replace_all`. The
/// replacement is first translated from vim's notation (`&`) to the
/// regex crate's (`$0`) so `$n` interpolation still runs.
fn regex_replace(
    regex: &regex::Regex,
    text: &str,
    replacement: &str,
    global: bool,
) -> (String, usize) {
    let matches = regex.find_iter(text).count();
    if matches == 0 {
        return (text.to_string(), 0);
    }
    let rep = expand_vim_replacement(replacement);
    let replaced = if global {
        regex.replace_all(text, rep.as_str()).into_owned()
    } else {
        regex.replace(text, rep.as_str()).into_owned()
    };
    let count = if global { matches } else { 1 };
    (replaced, count)
}

/// Translate vim-ish replacement placeholders to regex ones. For now only
/// `&` → the whole match; vim also supports `\0-\9` which the `regex`
/// crate already honours, so we leave those alone.
fn expand_vim_replacement(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                out.push('\\');
                out.push(next);
                chars.next();
            } else {
                out.push('\\');
            }
        } else if c == '&' {
            // `&` in vim replacement == whole match, same as `$0` for `regex`.
            out.push_str("$0");
        } else {
            out.push(c);
        }
    }
    out
}

impl<'a> Editor<'a> {
    /// Called by ex-command handlers after they rewrite the buffer.
    /// Ensures dirty tracking and undo bookkeeping stay consistent.
    fn mark_dirty_after_ex(&mut self) {
        self.content_dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Editor;
    use sqeel_core::state::KeybindingMode;

    fn new(content: &str) -> Editor<'static> {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(content);
        e
    }

    #[test]
    fn substitute_current_line() {
        let mut e = new("foo foo\nfoo foo");
        let effect = run(&mut e, "s/foo/bar/");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(e.textarea.lines()[0], "bar foo");
        assert_eq!(e.textarea.lines()[1], "foo foo");
    }

    #[test]
    fn substitute_current_line_global() {
        let mut e = new("foo foo\nfoo");
        run(&mut e, "s/foo/bar/g");
        assert_eq!(e.textarea.lines()[0], "bar bar");
        assert_eq!(e.textarea.lines()[1], "foo");
    }

    #[test]
    fn substitute_whole_buffer_global() {
        let mut e = new("foo\nfoo foo\nbar");
        let effect = run(&mut e, "%s/foo/xyz/g");
        assert_eq!(effect, ExEffect::Substituted { count: 3 });
        assert_eq!(e.textarea.lines()[0], "xyz");
        assert_eq!(e.textarea.lines()[1], "xyz xyz");
        assert_eq!(e.textarea.lines()[2], "bar");
    }

    #[test]
    fn substitute_zero_matches_reports_zero() {
        let mut e = new("hello");
        let effect = run(&mut e, "s/xyz/abc/");
        assert_eq!(effect, ExEffect::Substituted { count: 0 });
        assert_eq!(e.textarea.lines()[0], "hello");
    }

    #[test]
    fn substitute_respects_case_insensitive_flag() {
        let mut e = new("Foo");
        let effect = run(&mut e, "s/foo/bar/i");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(e.textarea.lines()[0], "bar");
    }

    #[test]
    fn substitute_accepts_alternate_separator() {
        let mut e = new("/usr/local/bin");
        run(&mut e, "s#/usr#/opt#");
        assert_eq!(e.textarea.lines()[0], "/opt/local/bin");
    }

    #[test]
    fn substitute_ampersand_in_replacement() {
        let mut e = new("foo");
        run(&mut e, "s/foo/[&]/");
        assert_eq!(e.textarea.lines()[0], "[foo]");
    }

    #[test]
    fn goto_line() {
        let mut e = new("a\nb\nc\nd");
        run(&mut e, "3");
        assert_eq!(e.textarea.cursor().0, 2);
    }

    #[test]
    fn quit_and_force_quit() {
        let mut e = new("");
        assert_eq!(
            run(&mut e, "q"),
            ExEffect::Quit {
                force: false,
                save: false
            }
        );
        assert_eq!(
            run(&mut e, "q!"),
            ExEffect::Quit {
                force: true,
                save: false
            }
        );
        assert_eq!(
            run(&mut e, "wq"),
            ExEffect::Quit {
                force: false,
                save: true
            }
        );
    }

    #[test]
    fn write_returns_save() {
        let mut e = new("");
        assert_eq!(run(&mut e, "w"), ExEffect::Save);
    }

    #[test]
    fn noh_is_ok() {
        let mut e = new("");
        assert_eq!(run(&mut e, "noh"), ExEffect::Ok);
    }

    #[test]
    fn unknown_command() {
        let mut e = new("");
        match run(&mut e, "blargh") {
            ExEffect::Unknown(cmd) => assert_eq!(cmd, "blargh"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn bad_substitute_pattern() {
        let mut e = new("hi");
        match run(&mut e, "s/[unterminated/foo/") {
            ExEffect::Error(_) => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn substitute_escaped_separator() {
        let mut e = new("a/b/c");
        let effect = run(&mut e, "s/\\//-/g");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(e.textarea.lines()[0], "a-b-c");
    }
}
