//! Backend-agnostic key input types used by the vim engine.
//!
//! Phase 8 of the sqeel-buffer migration replaced `tui_textarea::Input`
//! / `tui_textarea::Key` with these in-crate equivalents so the
//! editor can drop the `tui-textarea` dependency entirely.

/// A key code, mirroring the subset of [`crossterm::event::KeyCode`]
/// the vim engine actually consumes. `Null` is the conventional
/// sentinel for "no input" (matching the previous `tui_textarea::Key`
/// shape) so call sites can early-return on unsupported keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Tab,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Esc,
    #[default]
    Null,
}

/// A key press with modifier flags. The vim engine reads modifiers
/// directly off this struct (e.g. `input.ctrl && input.key == Key::Char('d')`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Input {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// Serialize a captured macro into vim's keystroke notation
/// (`<Esc>`, `<C-d>`, `<lt>`, etc.) so it can live as plain text in a
/// register slot. Used when `q{reg}` finishes recording.
pub fn encode_macro(inputs: &[Input]) -> String {
    let mut out = String::new();
    for input in inputs {
        match input.key {
            Key::Char(c) if input.ctrl => {
                out.push_str("<C-");
                out.push(c);
                out.push('>');
            }
            Key::Char(c) if input.alt => {
                out.push_str("<M-");
                out.push(c);
                out.push('>');
            }
            Key::Char('<') => out.push_str("<lt>"),
            Key::Char(c) => out.push(c),
            Key::Esc => out.push_str("<Esc>"),
            Key::Enter => out.push_str("<CR>"),
            Key::Backspace => out.push_str("<BS>"),
            Key::Tab => out.push_str("<Tab>"),
            Key::Up => out.push_str("<Up>"),
            Key::Down => out.push_str("<Down>"),
            Key::Left => out.push_str("<Left>"),
            Key::Right => out.push_str("<Right>"),
            Key::Delete => out.push_str("<Del>"),
            Key::Home => out.push_str("<Home>"),
            Key::End => out.push_str("<End>"),
            Key::PageUp => out.push_str("<PageUp>"),
            Key::PageDown => out.push_str("<PageDown>"),
            Key::Null => {}
        }
    }
    out
}

/// Reverse of [`encode_macro`] — parse the textual form back into
/// `Input` events for replay. Unknown `<…>` tags are dropped silently
/// so the caller can roundtrip text the user pasted into a register
/// without erroring out on partial matches.
pub fn decode_macro(s: &str) -> Vec<Input> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(Input {
                key: Key::Char(c),
                ..Input::default()
            });
            continue;
        }
        let mut tag = String::new();
        let mut closed = false;
        for ch in chars.by_ref() {
            if ch == '>' {
                closed = true;
                break;
            }
            tag.push(ch);
        }
        if !closed {
            // Stray `<` with no `>` — emit the literal so we don't
            // silently drop user text.
            out.push(Input {
                key: Key::Char('<'),
                ..Input::default()
            });
            for ch in tag.chars() {
                out.push(Input {
                    key: Key::Char(ch),
                    ..Input::default()
                });
            }
            continue;
        }
        let input = match tag.as_str() {
            "Esc" => Input {
                key: Key::Esc,
                ..Input::default()
            },
            "CR" => Input {
                key: Key::Enter,
                ..Input::default()
            },
            "BS" => Input {
                key: Key::Backspace,
                ..Input::default()
            },
            "Tab" => Input {
                key: Key::Tab,
                ..Input::default()
            },
            "Up" => Input {
                key: Key::Up,
                ..Input::default()
            },
            "Down" => Input {
                key: Key::Down,
                ..Input::default()
            },
            "Left" => Input {
                key: Key::Left,
                ..Input::default()
            },
            "Right" => Input {
                key: Key::Right,
                ..Input::default()
            },
            "Del" => Input {
                key: Key::Delete,
                ..Input::default()
            },
            "Home" => Input {
                key: Key::Home,
                ..Input::default()
            },
            "End" => Input {
                key: Key::End,
                ..Input::default()
            },
            "PageUp" => Input {
                key: Key::PageUp,
                ..Input::default()
            },
            "PageDown" => Input {
                key: Key::PageDown,
                ..Input::default()
            },
            "lt" => Input {
                key: Key::Char('<'),
                ..Input::default()
            },
            t if t.starts_with("C-") => {
                let Some(ch) = t.chars().nth(2) else {
                    continue;
                };
                Input {
                    key: Key::Char(ch),
                    ctrl: true,
                    ..Input::default()
                }
            }
            t if t.starts_with("M-") => {
                let Some(ch) = t.chars().nth(2) else {
                    continue;
                };
                Input {
                    key: Key::Char(ch),
                    alt: true,
                    ..Input::default()
                }
            }
            _ => continue,
        };
        out.push(input);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple_chars() {
        let keys = vec![
            Input {
                key: Key::Char('h'),
                ..Input::default()
            },
            Input {
                key: Key::Char('i'),
                ..Input::default()
            },
        ];
        let text = encode_macro(&keys);
        assert_eq!(text, "hi");
        assert_eq!(decode_macro(&text), keys);
    }

    #[test]
    fn roundtrip_with_special_keys_and_ctrl() {
        let keys = vec![
            Input {
                key: Key::Char('i'),
                ..Input::default()
            },
            Input {
                key: Key::Char('X'),
                ..Input::default()
            },
            Input {
                key: Key::Esc,
                ..Input::default()
            },
            Input {
                key: Key::Char('d'),
                ctrl: true,
                ..Input::default()
            },
        ];
        let text = encode_macro(&keys);
        assert_eq!(text, "iX<Esc><C-d>");
        assert_eq!(decode_macro(&text), keys);
    }

    #[test]
    fn roundtrip_literal_lt() {
        let keys = vec![Input {
            key: Key::Char('<'),
            ..Input::default()
        }];
        let text = encode_macro(&keys);
        assert_eq!(text, "<lt>");
        assert_eq!(decode_macro(&text), keys);
    }
}
