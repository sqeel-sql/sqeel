//! Vim-style register bank.
//!
//! Slots:
//! - `"` (unnamed) — written by every `y` / `d` / `c` / `x`; the
//!   default source for `p` / `P`.
//! - `"0` — the most recent **yank**. Deletes do not touch it, so
//!   `yw…dw…p` still pastes the original yank.
//! - `"1`–`"9` — small-delete ring. Each delete shifts the ring
//!   (newest at `"1`, oldest dropped off `"9`).
//! - `"a`–`"z` — named slots. A capital letter (`"A`…) appends to
//!   the matching lowercase slot, matching vim semantics.

#[derive(Default, Clone, Debug)]
pub struct Slot {
    pub text: String,
    pub linewise: bool,
}

impl Slot {
    fn new(text: String, linewise: bool) -> Self {
        Self { text, linewise }
    }
}

#[derive(Default, Debug)]
pub struct Registers {
    /// `"` — written by every yank / delete / change.
    pub unnamed: Slot,
    /// `"0` — last yank only.
    pub yank_zero: Slot,
    /// `"1`–`"9` — last 9 deletes (`"1` newest).
    pub delete_ring: [Slot; 9],
    /// `"a`–`"z` — named user registers.
    pub named: [Slot; 26],
    /// `"+` / `"*` — system clipboard register. Both selectors alias
    /// the same slot (matches the typical Linux/macOS/Windows setup
    /// where there's no separate primary selection in our pipeline).
    /// The host (sqeel-tui) syncs this slot from the OS clipboard
    /// before paste and from the slot back out on yank.
    pub clip: Slot,
}

impl Registers {
    /// Record a yank operation. Writes to `"`, `"0`, and (if
    /// `target` is set) the named slot.
    pub fn record_yank(&mut self, text: String, linewise: bool, target: Option<char>) {
        let slot = Slot::new(text, linewise);
        self.unnamed = slot.clone();
        self.yank_zero = slot.clone();
        if let Some(c) = target {
            self.write_named(c, slot);
        }
    }

    /// Record a delete / change. Writes to `"`, rotates the
    /// `"1`–`"9` ring, and (if `target` is set) the named slot.
    /// Empty deletes are dropped — vim doesn't pollute the ring
    /// with no-ops.
    pub fn record_delete(&mut self, text: String, linewise: bool, target: Option<char>) {
        if text.is_empty() {
            return;
        }
        let slot = Slot::new(text, linewise);
        self.unnamed = slot.clone();
        for i in (1..9).rev() {
            self.delete_ring[i] = self.delete_ring[i - 1].clone();
        }
        self.delete_ring[0] = slot.clone();
        if let Some(c) = target {
            self.write_named(c, slot);
        }
    }

    /// Read a register by its single-char selector. Returns `None`
    /// for unrecognised selectors.
    pub fn read(&self, reg: char) -> Option<&Slot> {
        match reg {
            '"' => Some(&self.unnamed),
            '0' => Some(&self.yank_zero),
            '1'..='9' => Some(&self.delete_ring[(reg as u8 - b'1') as usize]),
            'a'..='z' => Some(&self.named[(reg as u8 - b'a') as usize]),
            'A'..='Z' => Some(&self.named[(reg.to_ascii_lowercase() as u8 - b'a') as usize]),
            '+' | '*' => Some(&self.clip),
            _ => None,
        }
    }

    /// Replace the clipboard slot's contents — host hook for syncing
    /// from the OS clipboard before a paste from `"+` / `"*`.
    pub fn set_clipboard(&mut self, text: String, linewise: bool) {
        self.clip = Slot::new(text, linewise);
    }

    fn write_named(&mut self, c: char, slot: Slot) {
        if c.is_ascii_lowercase() {
            self.named[(c as u8 - b'a') as usize] = slot;
        } else if c.is_ascii_uppercase() {
            let idx = (c.to_ascii_lowercase() as u8 - b'a') as usize;
            let cur = &mut self.named[idx];
            cur.text.push_str(&slot.text);
            cur.linewise = slot.linewise || cur.linewise;
        } else if c == '+' || c == '*' {
            self.clip = slot;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yank_writes_unnamed_and_zero() {
        let mut r = Registers::default();
        r.record_yank("foo".into(), false, None);
        assert_eq!(r.read('"').unwrap().text, "foo");
        assert_eq!(r.read('0').unwrap().text, "foo");
    }

    #[test]
    fn delete_rotates_ring_and_skips_zero() {
        let mut r = Registers::default();
        r.record_yank("kept".into(), false, None);
        r.record_delete("d1".into(), false, None);
        r.record_delete("d2".into(), false, None);
        // Newest delete is "1.
        assert_eq!(r.read('1').unwrap().text, "d2");
        assert_eq!(r.read('2').unwrap().text, "d1");
        // "0 untouched by deletes.
        assert_eq!(r.read('0').unwrap().text, "kept");
        // Unnamed mirrors the latest write.
        assert_eq!(r.read('"').unwrap().text, "d2");
    }

    #[test]
    fn named_lowercase_overwrites_uppercase_appends() {
        let mut r = Registers::default();
        r.record_yank("hello ".into(), false, Some('a'));
        r.record_yank("world".into(), false, Some('A'));
        assert_eq!(r.read('a').unwrap().text, "hello world");
        // "A is just a write target; reading 'A' returns the same slot.
        assert_eq!(r.read('A').unwrap().text, "hello world");
    }

    #[test]
    fn empty_delete_is_dropped() {
        let mut r = Registers::default();
        r.record_delete("first".into(), false, None);
        r.record_delete(String::new(), false, None);
        assert_eq!(r.read('1').unwrap().text, "first");
        assert!(r.read('2').unwrap().text.is_empty());
    }

    #[test]
    fn unknown_selector_returns_none() {
        let r = Registers::default();
        assert!(r.read('?').is_none());
        assert!(r.read('!').is_none());
    }

    #[test]
    fn plus_and_star_alias_clipboard_slot() {
        let mut r = Registers::default();
        r.set_clipboard("payload".into(), false);
        assert_eq!(r.read('+').unwrap().text, "payload");
        assert_eq!(r.read('*').unwrap().text, "payload");
    }

    #[test]
    fn yank_to_plus_writes_clipboard_slot() {
        let mut r = Registers::default();
        r.record_yank("hi".into(), false, Some('+'));
        assert_eq!(r.read('+').unwrap().text, "hi");
        // Unnamed always mirrors the latest write.
        assert_eq!(r.read('"').unwrap().text, "hi");
    }
}
