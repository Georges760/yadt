//! Keystrokes, as yadt's commands see them.
//!
//! `App::handle_key` is written against this rather than blit's input events, so the tests
//! drive it directly and the command table reads the same whatever delivers the keys.
//! [`KeyEvent::from_input`] is the one place the two meet.

use blit::{Input, Key, KeyInput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    /// Control was held. The only modifier any command reads.
    pub ctrl: bool,
}

impl KeyEvent {
    pub const fn new(code: KeyCode) -> Self {
        Self { code, ctrl: false }
    }

    pub const fn ctrl(code: KeyCode) -> Self {
        Self { code, ctrl: true }
    }

    /// The keystroke an input event carries, if it is one yadt has a use for.
    ///
    /// Releases are dropped and repeats kept, so holding `j` scrolls. blit reports a plain
    /// character as `Text` and a modified one as a `Key`; both arrive here as `Char`, with
    /// Control recorded and any other modifier ignored.
    pub fn from_input(input: Input) -> Option<Self> {
        let (key, modifiers) = match input {
            Input::Text(c) => return Some(Self::new(KeyCode::Char(c))),
            Input::Key(KeyInput {
                key,
                modifiers,
                pressed: true,
                ..
            }) => (key, modifiers),
            _ => return None,
        };
        let code = match key {
            Key::Character(c) => KeyCode::Char(c),
            Key::Enter => KeyCode::Enter,
            Key::Escape => KeyCode::Esc,
            Key::Tab => KeyCode::Tab,
            Key::Backspace => KeyCode::Backspace,
            Key::ArrowUp => KeyCode::Up,
            Key::ArrowDown => KeyCode::Down,
            Key::ArrowLeft => KeyCode::Left,
            Key::ArrowRight => KeyCode::Right,
            Key::PageUp => KeyCode::PageUp,
            Key::PageDown => KeyCode::PageDown,
            Key::Home => KeyCode::Home,
            Key::End => KeyCode::End,
            Key::Delete | Key::Insert | Key::Function(_) => return None,
        };
        Some(Self {
            code,
            ctrl: modifiers.control(),
        })
    }
}

#[cfg(test)]
mod tests {
    use blit::Modifiers;

    use super::*;

    fn key(key: Key, modifiers: Modifiers, pressed: bool, repeat: bool) -> Input {
        Input::Key(KeyInput {
            key,
            modifiers,
            pressed,
            repeat,
        })
    }

    #[test]
    fn typed_text_is_a_plain_character() {
        assert_eq!(
            KeyEvent::from_input(Input::Text('G')),
            Some(KeyEvent::new(KeyCode::Char('G')))
        );
    }

    #[test]
    fn control_stays_attached_to_its_character() {
        let input = key(Key::Character('d'), Modifiers::CONTROL, true, false);
        assert_eq!(
            KeyEvent::from_input(input),
            Some(KeyEvent::ctrl(KeyCode::Char('d')))
        );
    }

    #[test]
    fn a_held_key_repeats_but_its_release_does_nothing() {
        let held = key(Key::ArrowDown, Modifiers::NONE, true, true);
        assert_eq!(
            KeyEvent::from_input(held),
            Some(KeyEvent::new(KeyCode::Down))
        );
        let released = key(Key::ArrowDown, Modifiers::NONE, false, false);
        assert_eq!(KeyEvent::from_input(released), None);
    }

    #[test]
    fn pointer_input_is_not_a_keystroke() {
        assert_eq!(KeyEvent::from_input(Input::PointerLeave), None);
    }
}
