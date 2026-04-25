//! Ex-command parser + executor.
//!
//! Parses the text after a leading `:` in the command-line prompt and
//! returns an [`ExEffect`] describing what the caller should do. Only the
//! editor-local effects (substitute, goto-line, clear-highlight) are
//! applied in-place against `Editor`; quit / save / unknown are returned
//! to the caller so the TUI loop can run them.

use crate::editor::Editor;

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

    // Strip a leading range (`5,10`, `.,$`, `'a,'b`, `%`). `range` is
    // None when the user typed no addresses; the handler defaults to
    // the command's natural scope (current line for `:s`, whole buffer
    // for `:sort` / `:g`). Resolution errors surface as ExEffect::Error.
    let (range, cmd) = match parse_range(cmd, editor) {
        Ok(pair) => pair,
        Err(e) => return ExEffect::Error(e),
    };

    // Bare line number — jump there. (Only when no range was parsed,
    // since `parse_range` already consumes a leading number as an
    // address; a bare `:5` falls through with `range = Some(5..=5)`
    // and an empty `cmd`.)
    if range.is_none() {
        if let Ok(line) = cmd.parse::<usize>() {
            editor.goto_line(line);
            return ExEffect::Ok;
        }
    } else if cmd.is_empty() {
        // `:5` jumps to line 5; `:5,10` lands on the start of the
        // range (vim's behaviour for a bare-range command).
        if let Some(r) = range {
            editor.goto_line(r.start_one_based());
            return ExEffect::Ok;
        }
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
            editor.buffer_mut().set_search_pattern(None);
            return ExEffect::Ok;
        }
        "reg" | "registers" => return ExEffect::Info(format_registers(editor)),
        "marks" => return ExEffect::Info(format_marks(editor)),
        "undo" | "u" => {
            crate::vim::do_undo(editor);
            return ExEffect::Ok;
        }
        "redo" | "red" => {
            crate::vim::do_redo(editor);
            return ExEffect::Ok;
        }
        "foldindent" | "foldi" => return apply_fold_indent(editor),
        "foldsyntax" | "folds" => return apply_fold_syntax(editor),
        _ => {}
    }

    // `:[range]sort[!][iun]` — defaults to the whole buffer when no
    // range is given.
    if let Some(rest) = cmd.strip_prefix("sort").or_else(|| cmd.strip_prefix("sor")) {
        return apply_sort(editor, range, rest);
    }

    // `:set [option ...]` — toggle / assign vim options. Range is
    // ignored (vim's `:set` doesn't accept one).
    if let Some(rest) = cmd
        .strip_prefix("set ")
        .or_else(|| cmd.strip_prefix("se "))
        .or(if cmd == "set" || cmd == "se" {
            Some("")
        } else {
            None
        })
    {
        return apply_set(editor, rest);
    }

    // `:[range]g/pat/cmd` and inverse `:v/pat/cmd`.
    if let Some((negate, rest)) = parse_global_prefix(cmd) {
        return apply_global(editor, range, rest, negate);
    }

    // `:[range]s/...` substitute. The legacy `:%s/...` form (no
    // separate range) still works because `%` is parsed by
    // `parse_range` above and consumed before we get here.
    if let Some(rest) = cmd.strip_prefix('s') {
        return match parse_substitute_body(rest) {
            Ok(sub) => match apply_substitute(editor, range, sub) {
                Ok(count) => ExEffect::Substituted { count },
                Err(e) => ExEffect::Error(e),
            },
            Err(e) => ExEffect::Error(e),
        };
    }

    // `:[range]d` — delete the range. Reuses :g/pat/d's row-drop loop.
    if cmd == "d" {
        return apply_delete_range(editor, range);
    }

    // `:r path` / `:read path` — insert file contents below the
    // current line. Range is currently ignored; vim's `:Nr file`
    // semantics (insert below row N) can land later if needed.
    if let Some(path) = cmd.strip_prefix("read ").or_else(|| cmd.strip_prefix("r ")) {
        return apply_read_file(editor, path.trim());
    }

    // `:[range]!cmd` — pipe rows through `cmd`, replace with stdout.
    // Without a range, `:!cmd` runs the command and surfaces stdout
    // as an Info toast (vim's `:!cmd` shows it in the message area).
    if let Some(shell_cmd) = cmd.strip_prefix('!') {
        return apply_shell_filter(editor, range, shell_cmd.trim());
    }

    ExEffect::Unknown(cmd.to_string())
}

/// `:foldsyntax` / `:folds` — apply the host-supplied syntax-tree
/// block ranges as closed folds. sqeel-tui calls
/// [`Editor::set_syntax_fold_ranges`] on every tree-sitter re-parse;
/// running this command consumes the latest snapshot. No-op when the
/// host hasn't pushed any ranges yet.
fn apply_fold_syntax(editor: &mut Editor<'_>) -> ExEffect {
    let ranges = editor.syntax_fold_ranges.clone();
    if ranges.is_empty() {
        return ExEffect::Info("no syntax block ranges available".into());
    }
    let count = ranges.len();
    for (start, end) in ranges {
        editor.buffer_mut().add_fold(start, end, true);
    }
    ExEffect::Info(format!("created {count} fold(s)"))
}

/// `:foldindent` / `:foldi` — derive folds from leading-whitespace runs
/// (vim's `foldmethod=indent`, fired manually because auto-fold-on-edit
/// is expensive). Each row whose successor is more deeply indented
/// becomes a fold opener; the fold extends to the row before indent
/// drops back to or below the opener's level.
fn apply_fold_indent(editor: &mut Editor<'_>) -> ExEffect {
    let lines = editor.buffer().lines().to_vec();
    let total = lines.len();
    if total == 0 {
        return ExEffect::Ok;
    }
    let indent =
        |line: &str| -> usize { line.chars().take_while(|c| *c == ' ' || *c == '\t').count() };
    let indents: Vec<usize> = lines.iter().map(|l| indent(l)).collect();
    let blank: Vec<bool> = lines.iter().map(|l| l.trim().is_empty()).collect();
    let mut new_folds: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 1 < total {
        if blank[i] {
            i += 1;
            continue;
        }
        let head_indent = indents[i];
        let mut j = i + 1;
        // Skip blanks adjacent to the head — they belong to the same
        // block so a fold can span across them.
        while j < total && blank[j] {
            j += 1;
        }
        if j >= total || indents[j] <= head_indent {
            i += 1;
            continue;
        }
        // We have a fold opener — walk forward until indent drops back
        // to <= head_indent on a non-blank row.
        let mut end = j;
        let mut k = j + 1;
        while k < total {
            if !blank[k] && indents[k] <= head_indent {
                break;
            }
            end = k;
            k += 1;
        }
        new_folds.push((i, end));
        // Step by one (not past `end`) so nested indented runs inside
        // the outer block also get their own fold.
        i += 1;
    }
    if new_folds.is_empty() {
        return ExEffect::Info("no indented blocks to fold".into());
    }
    let count = new_folds.len();
    for (start, end) in new_folds {
        editor.buffer_mut().add_fold(start, end, true);
    }
    ExEffect::Info(format!("created {count} fold(s)"))
}

/// `:[range]!cmd` — pipe the range through `cmd` (or run bare with no
/// range). With a range, the rows are joined with `\n`, fed via
/// stdin to `sh -c cmd`, and replaced with stdout. Without a range
/// the command runs detached and stdout returns as an Info toast.
fn apply_shell_filter(editor: &mut Editor<'_>, range: Option<Range>, cmd: &str) -> ExEffect {
    if cmd.is_empty() {
        return ExEffect::Error(":! needs a shell command".into());
    }
    use std::io::Write;
    use std::process::{Command, Stdio};

    if range.is_none() {
        // Bare `:!cmd` — run, no buffer change, surface stdout via Info.
        let output = Command::new("sh").arg("-c").arg(cmd).output();
        return match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                if stdout.is_empty() {
                    ExEffect::Info(format!("`{cmd}` exited 0"))
                } else {
                    ExEffect::Info(stdout)
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let trimmed = stderr.trim();
                let label = if trimmed.is_empty() {
                    "no stderr".to_string()
                } else {
                    trimmed.to_string()
                };
                ExEffect::Error(format!(
                    "command exited {} ({label})",
                    out.status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into())
                ))
            }
            Err(e) => ExEffect::Error(format!("cannot run `{cmd}`: {e}")),
        };
    }

    // Range supplied — pipe the rows through the command.
    let scope = Range::or_default(range, Range::whole(editor));
    let mut all_lines: Vec<String> = editor.buffer().lines().to_vec();
    let total = all_lines.len();
    if total == 0 {
        return ExEffect::Ok;
    }
    let bot = scope.end.min(total - 1);
    if scope.start > bot {
        return ExEffect::Ok;
    }
    let payload = all_lines[scope.start..=bot].join("\n");
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ExEffect::Error(format!("cannot spawn `{cmd}`: {e}")),
    };
    if let Some(stdin) = child.stdin.as_mut()
        && let Err(e) = stdin.write_all(payload.as_bytes())
    {
        return ExEffect::Error(format!("cannot write to `{cmd}`: {e}"));
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return ExEffect::Error(format!("`{cmd}` failed: {e}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        let label = if trimmed.is_empty() {
            "no stderr".to_string()
        } else {
            trimmed.to_string()
        };
        return ExEffect::Error(format!(
            "command exited {} ({label})",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into())
        ));
    }
    let stdout = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(_) => return ExEffect::Error("filter output was not UTF-8".into()),
    };
    let trimmed = stdout.strip_suffix('\n').unwrap_or(&stdout);
    let new_rows: Vec<String> = trimmed.split('\n').map(String::from).collect();

    editor.push_undo();
    let after: Vec<String> = all_lines.split_off(bot + 1);
    all_lines.truncate(scope.start);
    all_lines.extend(new_rows);
    all_lines.extend(after);
    editor.restore(all_lines, (scope.start, 0));
    editor.mark_dirty_after_ex();
    ExEffect::Ok
}

/// `:r file` — read `path` from disk and insert below the current
/// row. Cursor lands on the first row of the inserted content.
/// Failures (missing file, permission denied) surface as
/// `ExEffect::Error` toasts.
fn apply_read_file(editor: &mut Editor<'_>, path: &str) -> ExEffect {
    use sqeel_buffer::{Edit, Position};
    if path.is_empty() {
        return ExEffect::Error(":r needs a file path or `!cmd`".into());
    }
    // `:r !cmd` runs `cmd` through `sh -c` and inserts stdout. Same
    // security posture as running anything from a shell — the user
    // typed the command themselves.
    let content = if let Some(cmd) = path.strip_prefix('!') {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return ExEffect::Error(":r ! needs a shell command".into());
        }
        match std::process::Command::new("sh").arg("-c").arg(cmd).output() {
            Ok(out) if out.status.success() => match String::from_utf8(out.stdout) {
                Ok(s) => s,
                Err(_) => return ExEffect::Error("command output was not UTF-8".into()),
            },
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let trimmed = stderr.trim();
                let label = if trimmed.is_empty() {
                    "no stderr".to_string()
                } else {
                    trimmed.to_string()
                };
                return ExEffect::Error(format!(
                    "command exited {} ({label})",
                    out.status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into())
                ));
            }
            Err(e) => return ExEffect::Error(format!("cannot run `{cmd}`: {e}")),
        }
    } else {
        match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return ExEffect::Error(format!("cannot read `{path}`: {e}")),
        }
    };
    // Vim's `:r` inserts after the current row; trailing newline in
    // the file is dropped to avoid a stray blank tail (vim does the
    // same).
    let trimmed = content.strip_suffix('\n').unwrap_or(&content);
    editor.push_undo();
    let row = editor.cursor().0;
    let line_chars = editor
        .buffer()
        .line(row)
        .map(|l| l.chars().count())
        .unwrap_or(0);
    let insert_text = format!("\n{trimmed}");
    editor.mutate_edit(Edit::InsertStr {
        at: Position::new(row, line_chars),
        text: insert_text,
    });
    // Cursor lands on the first inserted row (row + 1) at col 0.
    editor.jump_cursor(row + 1, 0);
    editor.mark_dirty_after_ex();
    ExEffect::Ok
}

/// 0-based, inclusive line range over the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    start: usize,
    end: usize,
}

impl Range {
    fn whole(editor: &Editor<'_>) -> Self {
        let last = editor.buffer().lines().len().saturating_sub(1);
        Self {
            start: 0,
            end: last,
        }
    }

    fn single(row: usize) -> Self {
        Self {
            start: row,
            end: row,
        }
    }

    fn start_one_based(&self) -> usize {
        self.start + 1
    }

    fn or_default(opt: Option<Self>, default: Self) -> Self {
        opt.unwrap_or(default)
    }
}

/// Single ex-mode address: `5`, `.`, `$`, `'a`. No `+/-` offset arith
/// yet — keeps the parser tight.
#[derive(Debug, Clone, Copy)]
enum Address {
    Number(usize), // 1-based, as the user typed it
    Current,
    Last,
    Mark(char),
}

/// Strip a leading address from `s` and return it plus the remainder.
/// Returns `None` when `s` doesn't start with one — the caller treats
/// that as "no range provided".
fn parse_address(s: &str) -> Option<(Address, &str)> {
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    match first {
        '.' => Some((Address::Current, &s[1..])),
        '$' => Some((Address::Last, &s[1..])),
        '\'' => {
            let (_, mark) = chars.next()?;
            Some((Address::Mark(mark), &s[2..]))
        }
        '0'..='9' => {
            let mut end = 1;
            for (i, c) in s.char_indices().skip(1) {
                if c.is_ascii_digit() {
                    end = i + c.len_utf8();
                } else {
                    break;
                }
            }
            let n: usize = s[..end].parse().ok()?;
            Some((Address::Number(n), &s[end..]))
        }
        _ => None,
    }
}

/// Resolve a parsed address against the current editor state. Numeric
/// addresses are clamped to the buffer; bad marks return an error.
fn resolve_address(addr: Address, editor: &Editor<'_>) -> Result<usize, String> {
    let last = editor.buffer().lines().len().saturating_sub(1);
    match addr {
        Address::Number(n) => Ok(n.saturating_sub(1).min(last)),
        Address::Current => Ok(editor.cursor().0),
        Address::Last => Ok(last),
        Address::Mark(c) => editor
            .vim
            .marks
            .get(&c)
            .map(|(r, _)| (*r).min(last))
            .ok_or_else(|| format!("mark `{c}` not set")),
    }
}

/// Strip a leading range (`%`, `N`, `N,M`, `.,$`, `'a,'b`) from `cmd`.
/// Returns the resolved 0-based inclusive range plus the remainder.
fn parse_range<'a>(cmd: &'a str, editor: &Editor<'_>) -> Result<(Option<Range>, &'a str), String> {
    if let Some(rest) = cmd.strip_prefix('%') {
        return Ok((Some(Range::whole(editor)), rest));
    }
    let Some((start_addr, after_start)) = parse_address(cmd) else {
        return Ok((None, cmd));
    };
    let start = resolve_address(start_addr, editor)?;
    if let Some(after_comma) = after_start.strip_prefix(',') {
        let (end_addr, rest) =
            parse_address(after_comma).unwrap_or((Address::Number(start + 1), after_comma));
        let end = resolve_address(end_addr, editor)?;
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        return Ok((Some(Range { start: lo, end: hi }), rest));
    }
    Ok((Some(Range::single(start)), after_start))
}

/// `:[range]d` — drop every row in the range.
fn apply_delete_range(editor: &mut Editor<'_>, range: Option<Range>) -> ExEffect {
    use sqeel_buffer::{Edit, MotionKind, Position};
    let r = Range::or_default(range, Range::single(editor.cursor().0));
    let total = editor.buffer().row_count();
    if total == 0 {
        return ExEffect::Ok;
    }
    let bot = r.end.min(total.saturating_sub(1));
    if r.start > bot {
        return ExEffect::Ok;
    }
    editor.push_undo();
    // Delete bottom-up so row indices stay valid.
    for row in (r.start..=bot).rev() {
        if editor.buffer().row_count() == 1 {
            let line_chars = editor
                .buffer()
                .line(0)
                .map(|l| l.chars().count())
                .unwrap_or(0);
            if line_chars > 0 {
                editor.mutate_edit(Edit::DeleteRange {
                    start: Position::new(0, 0),
                    end: Position::new(0, line_chars),
                    kind: MotionKind::Char,
                });
            }
            continue;
        }
        editor.mutate_edit(Edit::DeleteRange {
            start: Position::new(row, 0),
            end: Position::new(row, 0),
            kind: MotionKind::Line,
        });
    }
    editor.mark_dirty_after_ex();
    ExEffect::Ok
}

/// Detect a `:g/pat/cmd`, `:g!/pat/cmd`, or `:v/pat/cmd` prefix.
/// Returns `(negate, body_after_prefix)` where `body_after_prefix`
/// still has the leading separator + pattern + cmd attached.
fn parse_global_prefix(cmd: &str) -> Option<(bool, &str)> {
    if let Some(rest) = cmd.strip_prefix("g!") {
        return Some((true, rest));
    }
    if let Some(rest) = cmd.strip_prefix('v') {
        return Some((true, rest));
    }
    if let Some(rest) = cmd.strip_prefix('g') {
        return Some((false, rest));
    }
    None
}

/// Run `:[range]g/pat/d` (or its negated variants). Walks the rows in
/// `range` (whole buffer when None), collects matches, then drops them
/// in reverse so row indices stay valid through the cascade of deletes.
fn apply_global(
    editor: &mut Editor<'_>,
    range: Option<Range>,
    body: &str,
    negate: bool,
) -> ExEffect {
    use sqeel_buffer::{Edit, MotionKind, Position};
    let mut chars = body.chars();
    let sep = match chars.next() {
        Some(c) => c,
        None => return ExEffect::Error("empty :g pattern".into()),
    };
    if sep.is_alphanumeric() || sep == '\\' {
        return ExEffect::Error("global needs a separator, e.g. :g/foo/d".into());
    }
    let rest: String = chars.collect();
    let parts = split_unescaped(&rest, sep);
    if parts.len() < 2 {
        return ExEffect::Error("global needs /pattern/cmd".into());
    }
    let pattern = unescape(&parts[0], sep);
    let cmd = parts[1].trim();
    if cmd != "d" {
        return ExEffect::Error(format!(":g supports only `d` today, got `{cmd}`"));
    }
    let regex = match regex::Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => return ExEffect::Error(format!("bad pattern: {e}")),
    };

    editor.push_undo();
    // Identify rows to drop (newest-first so multi-line drops don't
    // shift indices under us). Default to the whole buffer when no
    // range was supplied — matches vim's `:g/pat/d` (no range = `%`).
    let scope = Range::or_default(range, Range::whole(editor));
    let row_count = editor.buffer().row_count();
    let bot = scope.end.min(row_count.saturating_sub(1));
    let mut targets: Vec<usize> = Vec::new();
    for row in scope.start..=bot {
        let line = editor.buffer().line(row).unwrap_or("");
        let matches = regex.is_match(line);
        if matches != negate {
            targets.push(row);
        }
    }
    if targets.is_empty() {
        editor.undo_stack.pop();
        return ExEffect::Substituted { count: 0 };
    }
    let count = targets.len();
    for row in targets.iter().rev() {
        let row = *row;
        // Last row in a 1-row buffer can't be removed (Buffer keeps
        // the one-empty-row invariant); just clear it instead.
        if editor.buffer().row_count() == 1 {
            let line_chars = editor
                .buffer()
                .line(0)
                .map(|l| l.chars().count())
                .unwrap_or(0);
            if line_chars > 0 {
                editor.mutate_edit(Edit::DeleteRange {
                    start: Position::new(0, 0),
                    end: Position::new(0, line_chars),
                    kind: MotionKind::Char,
                });
            }
            continue;
        }
        editor.mutate_edit(Edit::DeleteRange {
            start: Position::new(row, 0),
            end: Position::new(row, 0),
            kind: MotionKind::Line,
        });
    }
    editor.mark_dirty_after_ex();
    ExEffect::Substituted { count }
}

/// `:set [opt ...]` body. Splits on whitespace and applies each token.
/// Bare `:set` reports the current values for the supported options.
fn apply_set(editor: &mut Editor<'_>, body: &str) -> ExEffect {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        let s = editor.settings();
        return ExEffect::Info(format!(
            "shiftwidth={}  tabstop={}  textwidth={}  ignorecase={}",
            s.shiftwidth,
            s.tabstop,
            s.textwidth,
            if s.ignore_case { "on" } else { "off" }
        ));
    }
    for token in trimmed.split_whitespace() {
        if let Err(e) = apply_set_token(editor, token) {
            return ExEffect::Error(e);
        }
    }
    ExEffect::Ok
}

/// Apply a single `:set` token. Supports `name=value`, bare `name`
/// (turns booleans on), and `noname` (turns booleans off).
fn apply_set_token(editor: &mut Editor<'_>, token: &str) -> Result<(), String> {
    if let Some((name, value)) = token.split_once('=') {
        let parsed: usize = value
            .parse()
            .map_err(|_| format!("bad value `{value}` for :set {name}"))?;
        match name {
            "shiftwidth" | "sw" => {
                if parsed == 0 {
                    return Err("shiftwidth must be > 0".into());
                }
                editor.settings_mut().shiftwidth = parsed;
            }
            "tabstop" | "ts" => {
                if parsed == 0 {
                    return Err("tabstop must be > 0".into());
                }
                editor.settings_mut().tabstop = parsed;
            }
            "textwidth" | "tw" => {
                if parsed == 0 {
                    return Err("textwidth must be > 0".into());
                }
                editor.settings_mut().textwidth = parsed;
            }
            other => return Err(format!("unknown :set option `{other}`")),
        }
        return Ok(());
    }
    let (name, value) = if let Some(rest) = token.strip_prefix("no") {
        (rest, false)
    } else {
        (token, true)
    };
    match name {
        "ignorecase" | "ic" => editor.settings_mut().ignore_case = value,
        // Booleans we don't (yet) honour: accept silently so :set lines
        // copied from a vimrc don't error out. `foldenable` falls here.
        "foldenable" | "fen" => {}
        other => return Err(format!("unknown :set option `{other}`")),
    }
    Ok(())
}

/// `:[range]sort[!][iun]` body — `flags` is whatever followed the
/// command name (e.g. `!u`, ` un`, `i`). Sorts only the rows in `range`
/// (or the whole buffer when None).
fn apply_sort(editor: &mut Editor<'_>, range: Option<Range>, flags: &str) -> ExEffect {
    let trimmed = flags.trim();
    let mut reverse = false;
    let mut unique = false;
    let mut numeric = false;
    let mut ignore_case = false;
    for c in trimmed.chars() {
        match c {
            '!' => reverse = true,
            'u' => unique = true,
            'n' => numeric = true,
            'i' => ignore_case = true,
            ' ' | '\t' => {}
            other => return ExEffect::Error(format!("bad :sort flag `{other}`")),
        }
    }

    let mut all_lines: Vec<String> = editor.buffer().lines().to_vec();
    let total = all_lines.len();
    if total == 0 {
        return ExEffect::Ok;
    }
    let scope = Range::or_default(range, Range::whole(editor));
    let bot = scope.end.min(total - 1);
    if scope.start > bot {
        return ExEffect::Ok;
    }
    // Sort only the slice in range; keep the rest of the buffer intact.
    let mut slice: Vec<String> = all_lines[scope.start..=bot].to_vec();
    if numeric {
        // Vim's `:sort n`: extract the first decimal integer (with
        // optional leading `-`) on each line; lines with no number
        // sort first, in original order.
        slice.sort_by_key(|l| extract_leading_number(l));
    } else if ignore_case {
        slice.sort_by_key(|s| s.to_lowercase());
    } else {
        slice.sort();
    }
    if reverse {
        slice.reverse();
    }
    if unique {
        let cmp_key = |s: &str| -> String {
            if ignore_case {
                s.to_lowercase()
            } else {
                s.to_string()
            }
        };
        let mut seen = std::collections::HashSet::new();
        slice.retain(|line| seen.insert(cmp_key(line)));
    }
    // Splice the sorted slice back. `unique` may have shortened it.
    let after: Vec<String> = all_lines.split_off(bot + 1);
    all_lines.truncate(scope.start);
    all_lines.extend(slice);
    all_lines.extend(after);

    editor.push_undo();
    editor.restore(all_lines, (scope.start, 0));
    editor.mark_dirty_after_ex();
    ExEffect::Ok
}

/// Parse the first signed decimal integer from `line` for `:sort n`.
/// Lines with no leading number sort as `i64::MIN` so they cluster at
/// the top, matching vim's behaviour.
fn extract_leading_number(line: &str) -> i64 {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_digit() && bytes[i] != b'-' {
        i += 1;
    }
    if i >= bytes.len() {
        return i64::MIN;
    }
    let mut j = i;
    if bytes[j] == b'-' {
        j += 1;
    }
    let start = j;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    if j == start {
        return i64::MIN;
    }
    line[i..j].parse().unwrap_or(i64::MIN)
}

/// `:reg` / `:registers` — tabular dump of every non-empty register slot.
fn format_registers(editor: &Editor<'_>) -> String {
    let r = editor.registers();
    let mut lines = vec!["--- Registers ---".to_string()];
    let mut push = |sel: &str, text: &str, linewise: bool| {
        if text.is_empty() {
            return;
        }
        let marker = if linewise { "L" } else { " " };
        lines.push(format!("{sel:<3} {marker} {}", display_register(text)));
    };
    push("\"\"", &r.unnamed.text, r.unnamed.linewise);
    push("\"0", &r.yank_zero.text, r.yank_zero.linewise);
    for (i, slot) in r.delete_ring.iter().enumerate() {
        let sel = format!("\"{}", i + 1);
        push(&sel, &slot.text, slot.linewise);
    }
    for (i, slot) in r.named.iter().enumerate() {
        let sel = format!("\"{}", (b'a' + i as u8) as char);
        push(&sel, &slot.text, slot.linewise);
    }
    if lines.len() == 1 {
        lines.push("(no registers set)".to_string());
    }
    lines.join("\n")
}

/// Escape control chars + truncate so a multi-line register fits a single row
/// of the toast table.
fn display_register(text: &str) -> String {
    let escaped: String = text
        .chars()
        .map(|c| match c {
            '\n' => "\\n".to_string(),
            '\t' => "\\t".to_string(),
            '\r' => "\\r".to_string(),
            c => c.to_string(),
        })
        .collect();
    const MAX: usize = 60;
    if escaped.chars().count() > MAX {
        let head: String = escaped.chars().take(MAX - 3).collect();
        format!("{head}...")
    } else {
        escaped
    }
}

/// `:marks` — list every set mark with `(line, col)`. Lines are 1-based to
/// match vim; cols are 0-based.
fn format_marks(editor: &Editor<'_>) -> String {
    let mut lines = vec!["--- Marks ---".to_string(), "mark  line  col".to_string()];
    let mut entries: Vec<(char, usize, usize)> = editor
        .vim
        .marks
        .iter()
        .map(|(c, (r, col))| (*c, *r, *col))
        .collect();
    // Uppercase / file marks live separately on Editor.
    entries.extend(editor.file_marks.iter().map(|(c, (r, col))| (*c, *r, *col)));
    entries.sort_by_key(|(c, _, _)| *c);
    for (c, r, col) in entries {
        lines.push(format!(" {c}    {:>4}  {col:>3}", r + 1));
    }
    if let Some((r, col)) = editor.vim.jump_back.last() {
        lines.push(format!(" '    {:>4}  {col:>3}", r + 1));
    }
    if let Some((r, col)) = editor.vim.last_edit_pos {
        lines.push(format!(" .    {:>4}  {col:>3}", r + 1));
    }
    if lines.len() == 2 {
        lines.push("(no marks set)".to_string());
    }
    lines.join("\n")
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
    range: Option<Range>,
    sub: Substitute,
) -> Result<usize, String> {
    // Explicit `i` flag wins, otherwise honour the global `:set
    // ignorecase` switch.
    let case_insensitive = sub.case_insensitive || editor.settings().ignore_case;
    let pattern = if case_insensitive {
        format!("(?i){}", sub.pattern)
    } else {
        sub.pattern.clone()
    };
    let regex = regex::Regex::new(&pattern).map_err(|e| format!("bad pattern: {e}"))?;

    editor.push_undo();

    // No range = current line only — matches vim's `:s` default.
    let scope = Range::or_default(range, Range::single(editor.cursor().0));
    let (range_start, range_end) = (scope.start, scope.end);

    let mut new_lines: Vec<String> = editor.buffer().lines().to_vec();
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

    // Apply the new content. Yank survives across loads since it's
    // owned by Editor now (was previously held by the textarea).
    editor.buffer_mut().replace_all(&new_lines.join("\n"));
    editor
        .buffer_mut()
        .set_cursor(sqeel_buffer::Position::new(range_start, 0));
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
        self.mark_content_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeybindingMode;
    use crate::editor::Editor;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn new(content: &str) -> Editor<'static> {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(content);
        e
    }

    fn type_keys(e: &mut Editor<'_>, keys: &str) {
        for c in keys.chars() {
            let ev = match c {
                '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                '\x1b' => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                ch => KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            };
            e.handle_key(ev);
        }
    }

    #[test]
    fn substitute_current_line() {
        let mut e = new("foo foo\nfoo foo");
        let effect = run(&mut e, "s/foo/bar/");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(e.buffer().lines()[0], "bar foo");
        assert_eq!(e.buffer().lines()[1], "foo foo");
    }

    #[test]
    fn substitute_current_line_global() {
        let mut e = new("foo foo\nfoo");
        run(&mut e, "s/foo/bar/g");
        assert_eq!(e.buffer().lines()[0], "bar bar");
        assert_eq!(e.buffer().lines()[1], "foo");
    }

    #[test]
    fn substitute_whole_buffer_global() {
        let mut e = new("foo\nfoo foo\nbar");
        let effect = run(&mut e, "%s/foo/xyz/g");
        assert_eq!(effect, ExEffect::Substituted { count: 3 });
        assert_eq!(e.buffer().lines()[0], "xyz");
        assert_eq!(e.buffer().lines()[1], "xyz xyz");
        assert_eq!(e.buffer().lines()[2], "bar");
    }

    #[test]
    fn substitute_zero_matches_reports_zero() {
        let mut e = new("hello");
        let effect = run(&mut e, "s/xyz/abc/");
        assert_eq!(effect, ExEffect::Substituted { count: 0 });
        assert_eq!(e.buffer().lines()[0], "hello");
    }

    #[test]
    fn substitute_respects_case_insensitive_flag() {
        let mut e = new("Foo");
        let effect = run(&mut e, "s/foo/bar/i");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(e.buffer().lines()[0], "bar");
    }

    #[test]
    fn substitute_accepts_alternate_separator() {
        let mut e = new("/usr/local/bin");
        run(&mut e, "s#/usr#/opt#");
        assert_eq!(e.buffer().lines()[0], "/opt/local/bin");
    }

    #[test]
    fn substitute_ampersand_in_replacement() {
        let mut e = new("foo");
        run(&mut e, "s/foo/[&]/");
        assert_eq!(e.buffer().lines()[0], "[foo]");
    }

    #[test]
    fn goto_line() {
        let mut e = new("a\nb\nc\nd");
        run(&mut e, "3");
        assert_eq!(e.cursor().0, 2);
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
    fn registers_lists_unnamed_and_named() {
        let mut e = new("hello world");
        // `yw` populates `"` and `"0`; `"ayw` also fills `"a`.
        type_keys(&mut e, "yw");
        type_keys(&mut e, "\"ayw");
        let info = match run(&mut e, "reg") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.starts_with("--- Registers ---"));
        assert!(info.contains("\"\""));
        assert!(info.contains("\"0"));
        assert!(info.contains("\"a"));
        // Alias resolves to same command.
        assert_eq!(run(&mut e, "registers"), ExEffect::Info(info));
    }

    #[test]
    fn registers_empty_state() {
        let mut e = new("hi");
        let info = match run(&mut e, "reg") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.contains("(no registers set)"));
    }

    #[test]
    fn marks_lists_user_and_special() {
        let mut e = new("alpha\nbeta\ngamma");
        type_keys(&mut e, "ma");
        type_keys(&mut e, "jjmb");
        // `iX<Esc>` produces a last_edit_pos.
        type_keys(&mut e, "iX");
        let info = match run(&mut e, "marks") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.starts_with("--- Marks ---"));
        assert!(info.contains(" a "));
        assert!(info.contains(" b "));
        assert!(info.contains(" . "));
    }

    #[test]
    fn undo_alias_reverses_last_change() {
        let mut e = new("hello");
        type_keys(&mut e, "Aworld\x1b");
        assert_eq!(e.buffer().lines()[0], "helloworld");
        assert_eq!(run(&mut e, "undo"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hello");
        // Short alias.
        type_keys(&mut e, "Awow\x1b");
        assert_eq!(e.buffer().lines()[0], "hellowow");
        assert_eq!(run(&mut e, "u"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hello");
    }

    #[test]
    fn redo_alias_reapplies_undone_change() {
        let mut e = new("hi");
        type_keys(&mut e, "Athere\x1b");
        assert_eq!(e.buffer().lines()[0], "hithere");
        run(&mut e, "undo");
        assert_eq!(e.buffer().lines()[0], "hi");
        assert_eq!(run(&mut e, "redo"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hithere");
        // Short alias.
        run(&mut e, "u");
        assert_eq!(run(&mut e, "red"), ExEffect::Ok);
        assert_eq!(e.buffer().lines()[0], "hithere");
    }

    #[test]
    fn marks_empty_state() {
        let mut e = new("hi");
        let info = match run(&mut e, "marks") {
            ExEffect::Info(s) => s,
            other => panic!("expected Info, got {other:?}"),
        };
        assert!(info.contains("(no marks set)"));
    }

    #[test]
    fn sort_alphabetical() {
        let mut e = new("banana\napple\ncherry");
        assert_eq!(run(&mut e, "sort"), ExEffect::Ok);
        assert_eq!(
            e.buffer().lines(),
            vec!["apple".to_string(), "banana".into(), "cherry".into()]
        );
    }

    #[test]
    fn sort_reverse_with_bang() {
        let mut e = new("apple\nbanana\ncherry");
        run(&mut e, "sort!");
        assert_eq!(
            e.buffer().lines(),
            vec!["cherry".to_string(), "banana".into(), "apple".into()]
        );
    }

    #[test]
    fn sort_unique() {
        let mut e = new("foo\nbar\nfoo\nbaz\nbar");
        run(&mut e, "sort u");
        assert_eq!(
            e.buffer().lines(),
            vec!["bar".to_string(), "baz".into(), "foo".into()]
        );
    }

    #[test]
    fn sort_numeric() {
        let mut e = new("10\n2\n100\n7");
        run(&mut e, "sort n");
        assert_eq!(
            e.buffer().lines(),
            vec!["2".to_string(), "7".into(), "10".into(), "100".into()]
        );
    }

    #[test]
    fn sort_ignore_case() {
        let mut e = new("Banana\napple\nCherry");
        run(&mut e, "sort i");
        assert_eq!(
            e.buffer().lines(),
            vec!["apple".to_string(), "Banana".into(), "Cherry".into()]
        );
    }

    #[test]
    fn sort_undo_restores_original_order() {
        let mut e = new("c\nb\na");
        run(&mut e, "sort");
        assert_eq!(e.buffer().lines()[0], "a");
        crate::vim::do_undo(&mut e);
        assert_eq!(
            e.buffer().lines(),
            vec!["c".to_string(), "b".into(), "a".into()]
        );
    }

    #[test]
    fn sort_rejects_unknown_flag() {
        let mut e = new("a\nb");
        match run(&mut e, "sortz") {
            ExEffect::Error(msg) => assert!(msg.contains("z")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn range_sort_partial() {
        // `:2,4sort` sorts rows 1..=3 (1-based 2..=4) only.
        let mut e = new("z\nc\nb\na\nx");
        run(&mut e, "2,4sort");
        assert_eq!(
            e.buffer().lines(),
            vec![
                "z".to_string(),
                "a".into(),
                "b".into(),
                "c".into(),
                "x".into(),
            ]
        );
    }

    #[test]
    fn range_substitute_partial() {
        let mut e = new("foo\nfoo\nfoo\nfoo");
        // `:2,3s/foo/bar/` only replaces lines 2 and 3.
        let effect = run(&mut e, "2,3s/foo/bar/");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(
            e.buffer().lines(),
            vec!["foo".to_string(), "bar".into(), "bar".into(), "foo".into(),]
        );
    }

    #[test]
    fn range_delete_drops_lines() {
        let mut e = new("a\nb\nc\nd\ne");
        run(&mut e, "2,4d");
        assert_eq!(e.buffer().lines(), vec!["a".to_string(), "e".into()]);
    }

    #[test]
    fn percent_substitute_still_works() {
        let mut e = new("foo\nfoo");
        let effect = run(&mut e, "%s/foo/bar/");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(e.buffer().lines(), vec!["bar".to_string(), "bar".into()]);
    }

    #[test]
    fn dot_dollar_addresses_resolve() {
        let mut e = new("a\nb\nc\nd");
        e.jump_cursor(1, 0);
        // `.,$d` deletes from the current row to the bottom.
        run(&mut e, ".,$d");
        assert_eq!(e.buffer().lines(), vec!["a".to_string()]);
    }

    #[test]
    fn mark_address_resolves() {
        let mut e = new("a\nb\nc\nd\ne");
        // Set marks `a` on row 1, `b` on row 3.
        e.jump_cursor(1, 0);
        type_keys(&mut e, "ma");
        e.jump_cursor(3, 0);
        type_keys(&mut e, "mb");
        run(&mut e, "'a,'bd");
        assert_eq!(e.buffer().lines(), vec!["a".to_string(), "e".into()]);
    }

    #[test]
    fn range_global_partial() {
        let mut e = new("foo\nfoo\nbar\nfoo\nfoo");
        // Only delete `foo` lines in rows 2..=4.
        run(&mut e, "2,4g/foo/d");
        assert_eq!(
            e.buffer().lines(),
            vec!["foo".to_string(), "bar".into(), "foo".into()]
        );
    }

    #[test]
    fn bare_line_number_jumps() {
        let mut e = new("a\nb\nc\nd");
        run(&mut e, "3");
        assert_eq!(e.cursor().0, 2);
    }

    #[test]
    fn set_shiftwidth_changes_indent_step() {
        let mut e = new("hello");
        // Default: shiftwidth = 2.
        run(&mut e, "set sw=4");
        assert_eq!(e.settings().shiftwidth, 4);
        // Indent uses the new value: `>>` prepends 4 spaces now.
        type_keys(&mut e, ">>");
        assert_eq!(e.buffer().lines()[0], "    hello");
    }

    #[test]
    fn set_tabstop_stored() {
        let mut e = new("");
        run(&mut e, "set tabstop=4");
        assert_eq!(e.settings().tabstop, 4);
    }

    #[test]
    fn set_ignorecase_affects_substitute() {
        let mut e = new("Hello");
        // Plain :s/h/X/ misses on the lowercase pattern.
        let effect = run(&mut e, "s/h/X/");
        assert_eq!(effect, ExEffect::Substituted { count: 0 });
        run(&mut e, "set ignorecase");
        assert!(e.settings().ignore_case);
        let effect = run(&mut e, "s/h/X/");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(e.buffer().lines()[0], "Xello");
    }

    #[test]
    fn set_no_prefix_disables_boolean() {
        let mut e = new("x");
        run(&mut e, "set ic");
        assert!(e.settings().ignore_case);
        run(&mut e, "set noic");
        assert!(!e.settings().ignore_case);
    }

    #[test]
    fn set_zero_shiftwidth_errors() {
        let mut e = new("x");
        match run(&mut e, "set sw=0") {
            ExEffect::Error(msg) => assert!(msg.contains("shiftwidth")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn set_unknown_option_errors() {
        let mut e = new("x");
        match run(&mut e, "set bogus") {
            ExEffect::Error(msg) => assert!(msg.contains("bogus")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn bare_set_reports_current_values() {
        let mut e = new("x");
        match run(&mut e, "set") {
            ExEffect::Info(msg) => {
                assert!(msg.contains("shiftwidth=2"));
                assert!(msg.contains("ignorecase=off"));
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn foldsyntax_applies_host_supplied_ranges() {
        let mut e = new("a\nb\nc\nd\ne");
        e.set_syntax_fold_ranges(vec![(0, 2), (3, 4)]);
        match run(&mut e, "foldsyntax") {
            ExEffect::Info(msg) => assert!(msg.contains("2 fold")),
            other => panic!("expected Info, got {other:?}"),
        }
        let folds = e.buffer().folds();
        assert_eq!(folds.len(), 2);
        assert!(folds.iter().any(|f| f.start_row == 0 && f.end_row == 2));
        assert!(folds.iter().any(|f| f.start_row == 3 && f.end_row == 4));
    }

    #[test]
    fn foldsyntax_no_ranges_reports_info() {
        let mut e = new("a\nb");
        match run(&mut e, "foldsyntax") {
            ExEffect::Info(msg) => assert!(msg.contains("no syntax block")),
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn foldsyntax_short_alias() {
        let mut e = new("a\nb\nc");
        e.set_syntax_fold_ranges(vec![(0, 2)]);
        assert!(matches!(run(&mut e, "folds"), ExEffect::Info(_)));
        assert_eq!(e.buffer().folds().len(), 1);
    }

    #[test]
    fn foldindent_creates_fold_for_indented_block() {
        let mut e = new("SELECT *\n  FROM t\n  WHERE x = 1\nORDER BY id");
        match run(&mut e, "foldindent") {
            ExEffect::Info(msg) => assert!(msg.contains("1 fold")),
            other => panic!("expected Info, got {other:?}"),
        }
        let folds = e.buffer().folds();
        assert_eq!(folds.len(), 1);
        assert_eq!(folds[0].start_row, 0);
        assert_eq!(folds[0].end_row, 2);
        assert!(folds[0].closed);
    }

    #[test]
    fn foldindent_no_blocks_reports_info() {
        let mut e = new("a\nb\nc");
        match run(&mut e, "foldindent") {
            ExEffect::Info(msg) => assert!(msg.contains("no indented blocks")),
            other => panic!("expected Info, got {other:?}"),
        }
        assert!(e.buffer().folds().is_empty());
    }

    #[test]
    fn foldindent_handles_nested_blocks() {
        let mut e = new("outer\n  mid\n    inner1\n    inner2\n  back\noutmost");
        run(&mut e, "foldindent");
        let folds = e.buffer().folds();
        // Outer block 0..=4 + inner block 1..=3 (mid → inner runs).
        assert_eq!(folds.len(), 2);
        assert_eq!(folds[0].start_row, 0);
        assert_eq!(folds[0].end_row, 4);
        assert_eq!(folds[1].start_row, 1);
        assert_eq!(folds[1].end_row, 3);
    }

    #[test]
    fn foldindent_skips_blanks_inside_block() {
        let mut e = new("head\n  body1\n\n  body2\nfoot");
        run(&mut e, "foldindent");
        let folds = e.buffer().folds();
        assert_eq!(folds.len(), 1);
        assert_eq!(folds[0].start_row, 0);
        assert_eq!(folds[0].end_row, 3);
    }

    #[test]
    fn foldindent_short_alias() {
        let mut e = new("a\n  b\nc");
        assert!(matches!(run(&mut e, "foldi"), ExEffect::Info(_)));
        assert_eq!(e.buffer().folds().len(), 1);
    }

    #[test]
    fn read_file_inserts_below_current_row() {
        // Write a temp file with two rows.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sqeel_read_{}.sql", std::process::id()));
        std::fs::write(&path, "SELECT 1;\nSELECT 2;\n").unwrap();
        let mut e = new("alpha\nbeta");
        e.jump_cursor(0, 0);
        let cmd = format!("r {}", path.display());
        assert_eq!(run(&mut e, &cmd), ExEffect::Ok);
        assert_eq!(
            e.buffer().lines(),
            vec![
                "alpha".to_string(),
                "SELECT 1;".into(),
                "SELECT 2;".into(),
                "beta".into(),
            ]
        );
        // Cursor sits on the first inserted row.
        assert_eq!(e.cursor(), (1, 0));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn shell_filter_replaces_range() {
        let mut e = new("c\nb\na");
        // `:%!sort` reorders the whole buffer alphabetically.
        assert_eq!(run(&mut e, "%!sort"), ExEffect::Ok);
        assert_eq!(
            e.buffer().lines(),
            vec!["a".to_string(), "b".into(), "c".into()]
        );
    }

    #[test]
    fn shell_filter_partial_range() {
        let mut e = new("head\ngamma\nbeta\nalpha\ntail");
        // `:2,4!sort` should reorder rows 2..=4 only.
        run(&mut e, "2,4!sort");
        assert_eq!(
            e.buffer().lines(),
            vec![
                "head".to_string(),
                "alpha".into(),
                "beta".into(),
                "gamma".into(),
                "tail".into(),
            ]
        );
    }

    #[test]
    fn shell_filter_undo_restores() {
        let mut e = new("c\nb\na");
        let before: Vec<String> = e.buffer().lines().to_vec();
        run(&mut e, "%!sort");
        crate::vim::do_undo(&mut e);
        assert_eq!(e.buffer().lines(), before);
    }

    #[test]
    fn shell_command_no_range_returns_info() {
        let mut e = new("buffer stays put");
        match run(&mut e, "!echo from-shell") {
            ExEffect::Info(msg) => assert!(msg.contains("from-shell")),
            other => panic!("expected Info, got {other:?}"),
        }
        // Buffer unchanged.
        assert_eq!(e.buffer().lines()[0], "buffer stays put");
    }

    #[test]
    fn shell_filter_failing_command_errors() {
        let mut e = new("a\nb");
        match run(&mut e, "%!exit 5") {
            ExEffect::Error(msg) => assert!(msg.contains("exited 5")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn shell_bang_empty_command_errors() {
        let mut e = new("a");
        match run(&mut e, "!") {
            ExEffect::Error(msg) => assert!(msg.contains("shell command")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn read_bang_inserts_command_stdout() {
        let mut e = new("alpha\nbeta");
        e.jump_cursor(0, 0);
        // `echo` is portable — outputs a trailing newline that
        // apply_read_file strips.
        assert_eq!(run(&mut e, "r !echo hello"), ExEffect::Ok);
        assert_eq!(
            e.buffer().lines(),
            vec!["alpha".to_string(), "hello".into(), "beta".into()]
        );
    }

    #[test]
    fn read_bang_failing_command_errors() {
        let mut e = new("hi");
        match run(&mut e, "r !exit 7") {
            ExEffect::Error(msg) => assert!(msg.contains("exited 7")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn read_bang_empty_command_errors() {
        let mut e = new("hi");
        match run(&mut e, "r !") {
            ExEffect::Error(msg) => assert!(msg.contains("shell command")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn read_file_alias_read_works() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sqeel_read_alias_{}.sql", std::process::id()));
        std::fs::write(&path, "x").unwrap();
        let mut e = new("");
        let cmd = format!("read {}", path.display());
        run(&mut e, &cmd);
        assert_eq!(e.buffer().lines(), vec!["".to_string(), "x".into()]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_file_missing_path_errors() {
        let mut e = new("a");
        match run(&mut e, "r /nonexistent/path/sqeel_test_xyzzy") {
            ExEffect::Error(msg) => assert!(msg.contains("cannot read")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn read_file_undo_restores() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sqeel_read_undo_{}.sql", std::process::id()));
        std::fs::write(&path, "ins\n").unwrap();
        let mut e = new("a\nb");
        e.jump_cursor(0, 0);
        run(&mut e, &format!("r {}", path.display()));
        assert_eq!(e.buffer().lines().len(), 3);
        crate::vim::do_undo(&mut e);
        assert_eq!(e.buffer().lines(), vec!["a".to_string(), "b".into()]);
        std::fs::remove_file(&path).ok();
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
        assert_eq!(e.buffer().lines()[0], "a-b-c");
    }

    #[test]
    fn global_delete_drops_matching_rows() {
        let mut e = new("keep1\nDROP1\nkeep2\nDROP2\nkeep3");
        let effect = run(&mut e, "g/DROP/d");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(
            e.buffer().lines(),
            &[
                "keep1".to_string(),
                "keep2".to_string(),
                "keep3".to_string()
            ]
        );
    }

    #[test]
    fn global_negated_drops_non_matching_rows() {
        let mut e = new("keep1\nother\nkeep2");
        let effect = run(&mut e, "v/keep/d");
        assert_eq!(effect, ExEffect::Substituted { count: 1 });
        assert_eq!(
            e.buffer().lines(),
            &["keep1".to_string(), "keep2".to_string()]
        );
    }

    #[test]
    fn global_with_regex_pattern() {
        let mut e = new("foo bar\nbaz qux\nfoo baz\nbaz");
        // Drop lines starting with "foo".
        let effect = run(&mut e, r"g/^foo/d");
        assert_eq!(effect, ExEffect::Substituted { count: 2 });
        assert_eq!(
            e.buffer().lines(),
            &["baz qux".to_string(), "baz".to_string()]
        );
    }

    #[test]
    fn global_no_matches_reports_zero() {
        let mut e = new("hello\nworld");
        let effect = run(&mut e, "g/xyz/d");
        assert_eq!(effect, ExEffect::Substituted { count: 0 });
        assert_eq!(e.buffer().lines().len(), 2);
    }

    #[test]
    fn global_unsupported_command_errors_out() {
        let mut e = new("foo\nbar");
        let effect = run(&mut e, "g/foo/p");
        assert!(matches!(effect, ExEffect::Error(_)));
    }
}
