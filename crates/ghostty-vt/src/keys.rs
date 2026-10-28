use std::ptr;

use ghostty_vt_sys as sys;

use crate::error::{Error, Result, check};
use crate::terminal::Term;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Mods(sys::GhosttyMods);

impl Mods {
    fn shift(self) -> bool {
        self.0 & sys::GHOSTTY_MODS_SHIFT != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPress {
    key: sys::GhosttyKey,
    mods: Mods,
    text: String,
    unshifted: Option<char>,
    /// Shift made the text (`A`, `?`): consumed, as a keyboard reports
    /// it, so the kitty protocol sends the text rather than shift and the
    /// key.
    shift_consumed: bool,
}

const NAMED: &[(&str, sys::GhosttyKey)] = &[
    ("enter", sys::GHOSTTY_KEY_ENTER),
    ("tab", sys::GHOSTTY_KEY_TAB),
    ("esc", sys::GHOSTTY_KEY_ESCAPE),
    ("backspace", sys::GHOSTTY_KEY_BACKSPACE),
    ("up", sys::GHOSTTY_KEY_ARROW_UP),
    ("down", sys::GHOSTTY_KEY_ARROW_DOWN),
    ("left", sys::GHOSTTY_KEY_ARROW_LEFT),
    ("right", sys::GHOSTTY_KEY_ARROW_RIGHT),
    ("home", sys::GHOSTTY_KEY_HOME),
    ("end", sys::GHOSTTY_KEY_END),
    ("pageup", sys::GHOSTTY_KEY_PAGE_UP),
    ("pagedown", sys::GHOSTTY_KEY_PAGE_DOWN),
    ("insert", sys::GHOSTTY_KEY_INSERT),
    ("delete", sys::GHOSTTY_KEY_DELETE),
];

const PUNCTUATION: &[(char, char, sys::GhosttyKey)] = &[
    ('`', '~', sys::GHOSTTY_KEY_BACKQUOTE),
    ('-', '_', sys::GHOSTTY_KEY_MINUS),
    ('=', '+', sys::GHOSTTY_KEY_EQUAL),
    ('[', '{', sys::GHOSTTY_KEY_BRACKET_LEFT),
    (']', '}', sys::GHOSTTY_KEY_BRACKET_RIGHT),
    ('\\', '|', sys::GHOSTTY_KEY_BACKSLASH),
    (';', ':', sys::GHOSTTY_KEY_SEMICOLON),
    ('\'', '"', sys::GHOSTTY_KEY_QUOTE),
    (',', '<', sys::GHOSTTY_KEY_COMMA),
    ('.', '>', sys::GHOSTTY_KEY_PERIOD),
    ('/', '?', sys::GHOSTTY_KEY_SLASH),
    ('1', '!', sys::GHOSTTY_KEY_DIGIT_1),
    ('2', '@', sys::GHOSTTY_KEY_DIGIT_2),
    ('3', '#', sys::GHOSTTY_KEY_DIGIT_3),
    ('4', '$', sys::GHOSTTY_KEY_DIGIT_4),
    ('5', '%', sys::GHOSTTY_KEY_DIGIT_5),
    ('6', '^', sys::GHOSTTY_KEY_DIGIT_6),
    ('7', '&', sys::GHOSTTY_KEY_DIGIT_7),
    ('8', '*', sys::GHOSTTY_KEY_DIGIT_8),
    ('9', '(', sys::GHOSTTY_KEY_DIGIT_9),
    ('0', ')', sys::GHOSTTY_KEY_DIGIT_0),
];

impl KeyPress {
    /// A key by name: modifiers joined with `+` (`ctrl`, `shift`, `alt`,
    /// `super`, in any order) and a key, `enter`, `tab`, `esc`,
    /// `backspace`, `space`, the arrows `up` `down` `left` `right`,
    /// `home`, `end`, `pageup`, `pagedown`, `insert`, `delete`, `f1` to
    /// `f12`, or a single character. Names ignore case except a single
    /// character's: `A` is `shift+a`. None for anything else.
    pub fn parse(name: &str) -> Option<KeyPress> {
        let mut parts: Vec<&str> = name.split('+').collect();
        let key = match parts.as_slice() {
            [.., "", ""] => {
                parts.truncate(parts.len() - 2);
                "+"
            }
            _ => parts.pop()?,
        };
        let mut mods = Mods::default();
        for part in parts {
            let bit = match part.to_ascii_lowercase().as_str() {
                "ctrl" => sys::GHOSTTY_MODS_CTRL,
                "shift" => sys::GHOSTTY_MODS_SHIFT,
                "alt" => sys::GHOSTTY_MODS_ALT,
                "super" => sys::GHOSTTY_MODS_SUPER,
                _ => return None,
            };
            if mods.0 & bit != 0 {
                return None;
            }
            mods.0 |= bit;
        }
        let lower = key.to_ascii_lowercase();
        if let Some((_, code)) = NAMED.iter().find(|(n, _)| *n == lower) {
            return Some(Self::textless(*code, mods));
        }
        if lower == "space" || key == " " {
            return Some(Self::character(sys::GHOSTTY_KEY_SPACE, mods, ' ', ' '));
        }
        if let Some(n) = lower
            .strip_prefix('f')
            .and_then(|n| n.parse::<sys::GhosttyKey>().ok())
            && (1..=12).contains(&n)
        {
            // The engine numbers F1 to F12 in order.
            return Some(Self::textless(sys::GHOSTTY_KEY_F1 + n - 1, mods));
        }
        let mut chars = key.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        if c.is_control() {
            return None;
        }
        if c.is_ascii_alphabetic() {
            let base = c.to_ascii_lowercase();
            let code = sys::GHOSTTY_KEY_A + sys::GhosttyKey::from(base as u8 - b'a');
            if c.is_ascii_uppercase() {
                mods.0 |= sys::GHOSTTY_MODS_SHIFT;
            }
            let text = if mods.shift() {
                base.to_ascii_uppercase()
            } else {
                base
            };
            return Some(Self::character(code, mods, text, base));
        }
        if let Some(&(plain, shifted, code)) = PUNCTUATION
            .iter()
            .find(|(plain, shifted, _)| *plain == c || *shifted == c)
        {
            if c == shifted {
                mods.0 |= sys::GHOSTTY_MODS_SHIFT;
            }
            let text = if mods.shift() { shifted } else { plain };
            return Some(Self::character(code, mods, text, plain));
        }
        Some(KeyPress {
            key: sys::GHOSTTY_KEY_UNIDENTIFIED,
            mods,
            text: c.to_string(),
            unshifted: Some(c),
            shift_consumed: false,
        })
    }

    fn textless(key: sys::GhosttyKey, mods: Mods) -> KeyPress {
        KeyPress {
            key,
            mods,
            text: String::new(),
            unshifted: None,
            shift_consumed: false,
        }
    }

    fn character(key: sys::GhosttyKey, mods: Mods, text: char, unshifted: char) -> KeyPress {
        KeyPress {
            key,
            mods,
            text: text.to_string(),
            unshifted: Some(unshifted),
            shift_consumed: mods.shift() && text != unshifted,
        }
    }
}

pub struct KeyEncoder {
    encoder: sys::GhosttyKeyEncoder,
    event: sys::GhosttyKeyEvent,
}

impl KeyEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder = ptr::null_mut();
        // SAFETY: a null allocator selects the default allocator.
        check(unsafe { sys::ghostty_key_encoder_new(ptr::null(), &raw mut encoder) })?;
        let mut event = ptr::null_mut();
        if let Err(err) = check(unsafe { sys::ghostty_key_event_new(ptr::null(), &raw mut event) })
        {
            // SAFETY: the encoder was just created and is freed once.
            unsafe { sys::ghostty_key_encoder_free(encoder) };
            return Err(err);
        }
        Ok(KeyEncoder { encoder, event })
    }

    pub fn encode(&mut self, term: &Term, key: &KeyPress) -> Result<Vec<u8>> {
        // SAFETY: encoder, event and terminal are live; the text outlives
        // the encode call that reads it.
        unsafe {
            sys::ghostty_key_encoder_setopt_from_terminal(self.encoder, term.raw());
            // A named `alt` is Alt, never macOS's Option composing text;
            // the call above resets this.
            let option_as_alt = sys::GHOSTTY_OPTION_AS_ALT_TRUE;
            sys::ghostty_key_encoder_setopt(
                self.encoder,
                sys::GHOSTTY_KEY_ENCODER_OPT_MACOS_OPTION_AS_ALT,
                (&raw const option_as_alt).cast(),
            );
            sys::ghostty_key_event_set_action(self.event, sys::GHOSTTY_KEY_ACTION_PRESS);
            sys::ghostty_key_event_set_key(self.event, key.key);
            sys::ghostty_key_event_set_mods(self.event, key.mods.0);
            let consumed = if key.shift_consumed {
                sys::GHOSTTY_MODS_SHIFT
            } else {
                0
            };
            sys::ghostty_key_event_set_consumed_mods(self.event, consumed);
            sys::ghostty_key_event_set_composing(self.event, false);
            let text = if key.text.is_empty() {
                ptr::null()
            } else {
                key.text.as_ptr().cast()
            };
            sys::ghostty_key_event_set_utf8(self.event, text, key.text.len());
            sys::ghostty_key_event_set_unshifted_codepoint(
                self.event,
                key.unshifted.map_or(0, u32::from),
            );
        }
        let mut buf = vec![0u8; 64];
        loop {
            let mut written = 0;
            // SAFETY: buf holds buf.len() writable bytes.
            let result = unsafe {
                sys::ghostty_key_encoder_encode(
                    self.encoder,
                    self.event,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    &raw mut written,
                )
            };
            match check(result) {
                Ok(()) => {
                    buf.truncate(written);
                    return Ok(buf);
                }
                Err(Error::OutOfSpace) if written > buf.len() => buf.resize(written, 0),
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for KeyEncoder {
    fn drop(&mut self) {
        // SAFETY: both were created by the library and are freed once.
        unsafe {
            sys::ghostty_key_event_free(self.event);
            sys::ghostty_key_encoder_free(self.encoder);
        }
    }
}

/// `data` as a paste: unsafe control bytes (ESC, NUL, DEL, ...) become
/// spaces, and the whole is bracketed when `bracketed` (mode 2004 is
/// set), else its newlines become carriage returns. The encoding the
/// engine gives a paste from a person.
pub fn encode_paste(data: &[u8], bracketed: bool) -> Result<Vec<u8>> {
    let mut input = data.to_vec();
    let mut out = vec![0u8; data.len() + 16];
    loop {
        let mut written = 0;
        // SAFETY: input and out hold their lengths in bytes; input may be
        // modified in place, and is a copy.
        let result = unsafe {
            sys::ghostty_paste_encode(
                input.as_mut_ptr().cast(),
                input.len(),
                bracketed,
                out.as_mut_ptr().cast(),
                out.len(),
                &raw mut written,
            )
        };
        match check(result) {
            Ok(()) => {
                out.truncate(written);
                return Ok(out);
            }
            Err(Error::OutOfSpace) if written > out.len() => {
                // The input was changed in place; start from a fresh copy.
                input = data.to_vec();
                out.resize(written, 0);
            }
            Err(err) => return Err(err),
        }
    }
}
