//! Rebindable input actions and the key-chord binding table.
//!
//! The render loop's input handler no longer hard-codes its keys: the
//! rebindable [`InputAction`]s each carry one [`KeyChord`] in a [`Keymap`], built
//! at startup from the built-in defaults plus any config overrides and carried on
//! the UI state. The browser-internal navigation (highlight up/down, accept) and
//! Esc's context-sensitive quit/cancel are *structural* behaviours, not actions,
//! so they are handled directly by the loop and are not listed here.

use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A runtime action a key can be bound to.
///
/// Every variant is rebindable from the config `[keys]` table. [`NowPlaying`]
/// toggles the now-playing panel (default `n`); [`Palette`] applies the current
/// track's art palette to the live scene, and reverts to the scene's own palette
/// when pressed again (default `p`).
///
/// [`NowPlaying`]: InputAction::NowPlaying
/// [`Palette`]: InputAction::Palette
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputAction {
    /// Cycle to the next scene (default `→`).
    SceneNext,
    /// Cycle to the previous scene (default `←`).
    ScenePrev,
    /// Toggle the scene browser overlay (default `tab`).
    Browser,
    /// Toggle the debug/performance overlay panel (default `` ` ``).
    Overlay,
    /// Freeze / unfreeze the scene (default `space`).
    Pause,
    /// Quit (default `q`).
    Quit,
    /// Cycle the chrome personality (default `c`).
    Chrome,
    /// Toggle the now-playing panel (default `n`).
    NowPlaying,
    /// Apply the current track's art palette to the live scene, toggling back to
    /// the scene's own palette on a second press (default `p`).
    Palette,
    /// Toggle the quick tuning strip (default `t`).
    Tuning,
    /// Toggle the expression-mapping overlay (default `m`).
    Mapping,
}

impl InputAction {
    /// Every action, in a stable order. [`Keymap::action_for`] scans this order,
    /// so an earlier action wins if two are ever bound to the same chord.
    pub const ALL: [InputAction; 11] = [
        InputAction::SceneNext,
        InputAction::ScenePrev,
        InputAction::Browser,
        InputAction::Overlay,
        InputAction::Pause,
        InputAction::Quit,
        InputAction::Chrome,
        InputAction::NowPlaying,
        InputAction::Palette,
        InputAction::Tuning,
        InputAction::Mapping,
    ];

    /// The config `[keys]` name this action is bound under.
    #[must_use]
    pub fn config_name(self) -> &'static str {
        match self {
            InputAction::SceneNext => "scene_next",
            InputAction::ScenePrev => "scene_prev",
            InputAction::Browser => "browser",
            InputAction::Overlay => "overlay",
            InputAction::Pause => "pause",
            InputAction::Quit => "quit",
            InputAction::Chrome => "chrome",
            InputAction::NowPlaying => "now_playing",
            InputAction::Palette => "palette",
            InputAction::Tuning => "tuning",
            InputAction::Mapping => "mapping",
        }
    }

    /// A short human label for the in-app help.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            InputAction::SceneNext => "next scene",
            InputAction::ScenePrev => "prev scene",
            InputAction::Browser => "scene browser",
            InputAction::Overlay => "debug overlay",
            InputAction::Pause => "pause",
            InputAction::Quit => "quit",
            InputAction::Chrome => "chrome mode",
            InputAction::NowPlaying => "now playing",
            InputAction::Palette => "apply palette",
            InputAction::Tuning => "tuning strip",
            InputAction::Mapping => "expression map",
        }
    }

    /// Parse a config `[keys]` action name. Unknown names yield `None` so the
    /// caller can warn and ignore rather than abort.
    #[must_use]
    pub fn parse(name: &str) -> Option<InputAction> {
        InputAction::ALL
            .into_iter()
            .find(|a| a.config_name() == name)
    }
}

/// A single key binding: a [`KeyCode`] plus whether Ctrl is held. The supported
/// config syntax is a single character, a named key, or `ctrl+<key>`, so Ctrl is
/// the only modifier tracked; Shift/Alt are ignored on match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    /// The key itself.
    pub code: KeyCode,
    /// Whether Ctrl must be held.
    pub ctrl: bool,
}

impl KeyChord {
    /// A chord with no modifier.
    #[must_use]
    pub const fn plain(code: KeyCode) -> Self {
        Self { code, ctrl: false }
    }

    /// A `ctrl+<key>` chord.
    #[must_use]
    pub const fn ctrl(code: KeyCode) -> Self {
        Self { code, ctrl: true }
    }

    /// Whether `key` triggers this chord: the code matches and the Ctrl state
    /// agrees. Other modifiers are not considered.
    #[must_use]
    pub fn matches(self, key: &KeyEvent) -> bool {
        key.code == self.code && key.modifiers.contains(KeyModifiers::CONTROL) == self.ctrl
    }

    /// Render the chord in config / help syntax, e.g. `ctrl+c`, `tab`, `space`,
    /// or `` ` ``.
    #[must_use]
    pub fn display(self) -> String {
        let base = match self.code {
            KeyCode::Tab => "tab".to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            other => format!("{other:?}").to_lowercase(),
        };
        if self.ctrl {
            format!("ctrl+{base}")
        } else {
            base
        }
    }
}

/// A key string that could not be parsed into a [`KeyChord`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChordParseError {
    /// The offending input, verbatim.
    pub input: String,
}

impl fmt::Display for ChordParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot parse key `{}`", self.input)
    }
}

impl std::error::Error for ChordParseError {}

/// Parse a key string into a [`KeyChord`].
///
/// Accepts a single character (`"q"`), a named key (`tab`, `esc`, `left`,
/// `right`, `up`, `down`, `enter`, `space`, `backtick`), or a `ctrl+<key>`
/// combination. Case-insensitive.
///
/// # Errors
/// [`ChordParseError`] when the string names no key.
pub fn parse_chord(s: &str) -> Result<KeyChord, ChordParseError> {
    let err = || ChordParseError {
        input: s.to_string(),
    };
    let lower = s.trim().to_ascii_lowercase();
    let (ctrl, rest) = match lower.strip_prefix("ctrl+") {
        Some(rest) => (true, rest),
        None => (false, lower.as_str()),
    };
    let code = parse_key_code(rest).ok_or_else(err)?;
    Ok(KeyChord { code, ctrl })
}

/// Map a (lowercased, un-prefixed) key name to a [`KeyCode`].
fn parse_key_code(name: &str) -> Option<KeyCode> {
    Some(match name {
        "tab" => KeyCode::Tab,
        "esc" => KeyCode::Esc,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "enter" => KeyCode::Enter,
        "space" => KeyCode::Char(' '),
        "backtick" => KeyCode::Char('`'),
        other => {
            let mut chars = other.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    })
}

/// The binding table: one optional [`KeyChord`] per [`InputAction`].
///
/// [`Keymap::default`] is the built-in binding set, matching the frontend's
/// long-standing hard-coded keys exactly (plus the new `space` = pause). The
/// config layer rebinds individual actions on top with [`rebind`](Self::rebind).
///
/// It is `Copy` so it can live on the UI state without ceremony.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keymap {
    /// Cycle to the next scene.
    pub scene_next: Option<KeyChord>,
    /// Cycle to the previous scene.
    pub scene_prev: Option<KeyChord>,
    /// Toggle the scene browser.
    pub browser: Option<KeyChord>,
    /// Toggle the debug/performance overlay.
    pub overlay: Option<KeyChord>,
    /// Freeze / unfreeze the scene.
    pub pause: Option<KeyChord>,
    /// Quit.
    pub quit: Option<KeyChord>,
    /// Cycle the chrome personality.
    pub chrome: Option<KeyChord>,
    /// Toggle the now-playing panel.
    pub now_playing: Option<KeyChord>,
    /// Apply / revert the current track's art palette.
    pub palette: Option<KeyChord>,
    /// Toggle the quick tuning strip.
    pub tuning: Option<KeyChord>,
    /// Toggle the expression-mapping overlay.
    pub mapping: Option<KeyChord>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            scene_next: Some(KeyChord::plain(KeyCode::Right)),
            scene_prev: Some(KeyChord::plain(KeyCode::Left)),
            browser: Some(KeyChord::plain(KeyCode::Tab)),
            overlay: Some(KeyChord::plain(KeyCode::Char('`'))),
            pause: Some(KeyChord::plain(KeyCode::Char(' '))),
            quit: Some(KeyChord::plain(KeyCode::Char('q'))),
            chrome: Some(KeyChord::plain(KeyCode::Char('c'))),
            now_playing: Some(KeyChord::plain(KeyCode::Char('n'))),
            palette: Some(KeyChord::plain(KeyCode::Char('p'))),
            tuning: Some(KeyChord::plain(KeyCode::Char('t'))),
            mapping: Some(KeyChord::plain(KeyCode::Char('m'))),
        }
    }
}

impl Keymap {
    /// The chord bound to `action`, if any.
    #[must_use]
    pub fn get(&self, action: InputAction) -> Option<KeyChord> {
        match action {
            InputAction::SceneNext => self.scene_next,
            InputAction::ScenePrev => self.scene_prev,
            InputAction::Browser => self.browser,
            InputAction::Overlay => self.overlay,
            InputAction::Pause => self.pause,
            InputAction::Quit => self.quit,
            InputAction::Chrome => self.chrome,
            InputAction::NowPlaying => self.now_playing,
            InputAction::Palette => self.palette,
            InputAction::Tuning => self.tuning,
            InputAction::Mapping => self.mapping,
        }
    }

    /// Rebind `action` to `chord` (or `None` to leave it unbound).
    pub fn rebind(&mut self, action: InputAction, chord: Option<KeyChord>) {
        let slot = match action {
            InputAction::SceneNext => &mut self.scene_next,
            InputAction::ScenePrev => &mut self.scene_prev,
            InputAction::Browser => &mut self.browser,
            InputAction::Overlay => &mut self.overlay,
            InputAction::Pause => &mut self.pause,
            InputAction::Quit => &mut self.quit,
            InputAction::Chrome => &mut self.chrome,
            InputAction::NowPlaying => &mut self.now_playing,
            InputAction::Palette => &mut self.palette,
            InputAction::Tuning => &mut self.tuning,
            InputAction::Mapping => &mut self.mapping,
        };
        *slot = chord;
    }

    /// The action `key` triggers, if any. Scans [`InputAction::ALL`] in order, so
    /// the first action bound to a matching chord wins.
    #[must_use]
    pub fn action_for(&self, key: &KeyEvent) -> Option<InputAction> {
        InputAction::ALL
            .into_iter()
            .find(|&a| self.get(a).is_some_and(|c| c.matches(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode, ctrl: bool) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: if ctrl {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            },
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn parses_single_chars() {
        assert_eq!(parse_chord("q"), Ok(KeyChord::plain(KeyCode::Char('q'))));
        assert_eq!(parse_chord("n"), Ok(KeyChord::plain(KeyCode::Char('n'))));
        // A literal backtick and its name both resolve.
        assert_eq!(parse_chord("`"), Ok(KeyChord::plain(KeyCode::Char('`'))));
        assert_eq!(
            parse_chord("backtick"),
            Ok(KeyChord::plain(KeyCode::Char('`')))
        );
    }

    #[test]
    fn parses_named_keys() {
        assert_eq!(parse_chord("tab"), Ok(KeyChord::plain(KeyCode::Tab)));
        assert_eq!(parse_chord("esc"), Ok(KeyChord::plain(KeyCode::Esc)));
        assert_eq!(parse_chord("left"), Ok(KeyChord::plain(KeyCode::Left)));
        assert_eq!(parse_chord("right"), Ok(KeyChord::plain(KeyCode::Right)));
        assert_eq!(parse_chord("up"), Ok(KeyChord::plain(KeyCode::Up)));
        assert_eq!(parse_chord("down"), Ok(KeyChord::plain(KeyCode::Down)));
        assert_eq!(parse_chord("enter"), Ok(KeyChord::plain(KeyCode::Enter)));
        assert_eq!(
            parse_chord("space"),
            Ok(KeyChord::plain(KeyCode::Char(' ')))
        );
    }

    #[test]
    fn parses_ctrl_combos_case_insensitively() {
        assert_eq!(
            parse_chord("ctrl+c"),
            Ok(KeyChord::ctrl(KeyCode::Char('c')))
        );
        assert_eq!(
            parse_chord("CTRL+C"),
            Ok(KeyChord::ctrl(KeyCode::Char('c')))
        );
        assert_eq!(parse_chord("Ctrl+Tab"), Ok(KeyChord::ctrl(KeyCode::Tab)));
    }

    #[test]
    fn rejects_unparseable_keys() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("abc").is_err());
        assert!(parse_chord("ctrl+").is_err());
        assert!(parse_chord("f13").is_err());
    }

    #[test]
    fn action_names_round_trip() {
        for a in InputAction::ALL {
            assert_eq!(InputAction::parse(a.config_name()), Some(a));
        }
        assert_eq!(InputAction::parse("nope"), None);
    }

    #[test]
    fn default_map_matches_the_historic_bindings() {
        let km = Keymap::default();
        assert!(km.quit.unwrap().matches(&key(KeyCode::Char('q'), false)));
        assert!(km.browser.unwrap().matches(&key(KeyCode::Tab, false)));
        assert!(km.overlay.unwrap().matches(&key(KeyCode::Char('`'), false)));
        assert!(km.scene_next.unwrap().matches(&key(KeyCode::Right, false)));
        assert!(km.scene_prev.unwrap().matches(&key(KeyCode::Left, false)));
        assert!(km.pause.unwrap().matches(&key(KeyCode::Char(' '), false)));
        // The now-playing panel and palette-apply keys ship bound by default.
        assert!(
            km.now_playing
                .unwrap()
                .matches(&key(KeyCode::Char('n'), false))
        );
        assert!(km.palette.unwrap().matches(&key(KeyCode::Char('p'), false)));
    }

    #[test]
    fn now_playing_and_palette_resolve_to_their_actions() {
        let km = Keymap::default();
        assert_eq!(
            km.action_for(&key(KeyCode::Char('n'), false)),
            Some(InputAction::NowPlaying)
        );
        assert_eq!(
            km.action_for(&key(KeyCode::Char('p'), false)),
            Some(InputAction::Palette)
        );
    }

    #[test]
    fn tuning_defaults_to_t_and_rebinds() {
        let km = Keymap::default();
        assert!(km.tuning.unwrap().matches(&key(KeyCode::Char('t'), false)));
        assert_eq!(
            km.action_for(&key(KeyCode::Char('t'), false)),
            Some(InputAction::Tuning)
        );
        // The config name round-trips like every other action.
        assert_eq!(InputAction::parse("tuning"), Some(InputAction::Tuning));

        // Rebinding moves it to a new key and frees the old one.
        let mut km = Keymap::default();
        km.rebind(
            InputAction::Tuning,
            Some(KeyChord::plain(KeyCode::Char('g'))),
        );
        assert_eq!(
            km.action_for(&key(KeyCode::Char('g'), false)),
            Some(InputAction::Tuning)
        );
        assert_eq!(km.action_for(&key(KeyCode::Char('t'), false)), None);
    }

    #[test]
    fn mapping_defaults_to_m_and_rebinds() {
        let km = Keymap::default();
        assert!(km.mapping.unwrap().matches(&key(KeyCode::Char('m'), false)));
        assert_eq!(
            km.action_for(&key(KeyCode::Char('m'), false)),
            Some(InputAction::Mapping)
        );
        assert_eq!(InputAction::parse("mapping"), Some(InputAction::Mapping));

        // Rebinding moves it and frees the old key.
        let mut km = Keymap::default();
        km.rebind(
            InputAction::Mapping,
            Some(KeyChord::plain(KeyCode::Char('x'))),
        );
        assert_eq!(
            km.action_for(&key(KeyCode::Char('x'), false)),
            Some(InputAction::Mapping)
        );
        assert_eq!(km.action_for(&key(KeyCode::Char('m'), false)), None);
    }

    #[test]
    fn action_for_resolves_and_respects_ctrl() {
        let km = Keymap::default();
        assert_eq!(
            km.action_for(&key(KeyCode::Char('q'), false)),
            Some(InputAction::Quit)
        );
        assert_eq!(
            km.action_for(&key(KeyCode::Right, false)),
            Some(InputAction::SceneNext)
        );
        // Ctrl+q is not plain q, so it matches nothing in the default map.
        assert_eq!(km.action_for(&key(KeyCode::Char('q'), true)), None);
        // An unbound key resolves to nothing.
        assert_eq!(km.action_for(&key(KeyCode::Char('z'), false)), None);
    }

    #[test]
    fn rebind_moves_the_binding() {
        let mut km = Keymap::default();
        km.rebind(
            InputAction::SceneNext,
            Some(KeyChord::plain(KeyCode::Char('n'))),
        );
        // The new key triggers scene_next.
        assert_eq!(
            km.action_for(&key(KeyCode::Char('n'), false)),
            Some(InputAction::SceneNext)
        );
        // The old key no longer does.
        assert_eq!(km.action_for(&key(KeyCode::Right, false)), None);
        // Unbinding clears it entirely.
        km.rebind(InputAction::Quit, None);
        assert_eq!(km.action_for(&key(KeyCode::Char('q'), false)), None);
    }
}
