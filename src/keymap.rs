//! Keys a view answers to, as `config.toml` may rebind them. Each view names
//! its actions and their default keys; `[keys.<view>]` replaces the keys of
//! the actions it names and leaves the rest as they were.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// What a view can do from a key. `TABLE` lists every action once, by the
/// name `config.toml` calls it, with its default keys. A key bound to several
/// actions tries them in the table's order, so the view can pass on an action
/// that does nothing at the moment to the next one.
pub trait Action: Copy + Eq + 'static {
    /// The `[keys.<VIEW>]` table that rebinds these actions.
    const VIEW: &'static str;
    const TABLE: &'static [(&'static str, Self, &'static [&'static str])];
}

/// One key with the modifiers held for it, such as `ctrl+d` or `G`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    code: KeyCode,
    mods: KeyModifiers,
}

/// The modifiers a chord can ask for. Shift is the letter's case for typed
/// characters, which terminals report either way.
const HELD: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

impl Chord {
    /// Reads `ctrl+d`, `alt+enter`, `shift+tab`, `G`, `space`, `+` and the like.
    pub fn parse(text: &str) -> Result<Chord, String> {
        let bad = |why: &str| format!("`{text}` is not a key: {why}");
        if text.is_empty() {
            return Err(bad("it is empty"));
        }
        // The key comes last, and may itself be `+`.
        let (mods_text, key) = match text.strip_suffix('+') {
            Some(rest) if rest.is_empty() || rest.ends_with('+') => {
                (rest.strip_suffix('+').unwrap_or(rest), "+")
            }
            _ => text.rsplit_once('+').unwrap_or(("", text)),
        };
        let mut mods = KeyModifiers::NONE;
        for name in mods_text.split('+').filter(|name| !name.is_empty()) {
            mods |= match name.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "option" | "opt" | "meta" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                other => {
                    return Err(bad(&format!(
                        "`{other}` is not a modifier; use ctrl, alt or shift"
                    )));
                }
            };
        }
        let mut chars = key.chars();
        let code = match (chars.next(), chars.next()) {
            (Some(c), None) => KeyCode::Char(c),
            _ => match key.to_ascii_lowercase().as_str() {
                "space" => KeyCode::Char(' '),
                "enter" | "return" => KeyCode::Enter,
                "esc" | "escape" => KeyCode::Esc,
                "tab" => KeyCode::Tab,
                "backtab" => KeyCode::BackTab,
                "backspace" => KeyCode::Backspace,
                "delete" | "del" => KeyCode::Delete,
                "insert" => KeyCode::Insert,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" | "pgup" => KeyCode::PageUp,
                "pagedown" | "pgdn" => KeyCode::PageDown,
                name => match name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                    Some(n @ 1..=12) => KeyCode::F(n),
                    _ => {
                        return Err(bad(
                            "name one character, or space, enter, esc, tab, backspace, delete, \
                             insert, up, down, left, right, home, end, pageup, pagedown or f1–f12",
                        ));
                    }
                },
            },
        };
        Ok(Chord::new(code, mods))
    }

    /// Folds shift into the key it changes: `shift+g` is `G`, `shift+tab` is backtab.
    fn new(code: KeyCode, mods: KeyModifiers) -> Chord {
        let shift = mods.contains(KeyModifiers::SHIFT);
        let code = match code {
            KeyCode::Char(c) if shift => KeyCode::Char(c.to_ascii_uppercase()),
            KeyCode::Tab if shift => KeyCode::BackTab,
            code => code,
        };
        let mods = match code {
            KeyCode::Char(_) | KeyCode::BackTab => mods & HELD,
            _ => mods & (HELD | KeyModifiers::SHIFT),
        };
        Chord { code, mods }
    }

    fn of(key: &KeyEvent) -> Chord {
        Chord::new(key.code, key.modifiers)
    }

    /// How the help screens show it: `^d`, `⌥enter`, `↓`, `space`.
    pub fn label(self) -> String {
        let mut label = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            label.push('^');
        }
        if self.mods.contains(KeyModifiers::ALT) {
            label.push('⌥');
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            label.push('⇧');
        }
        match self.code {
            KeyCode::Char(' ') => label.push_str("space"),
            KeyCode::Char(c) => label.push(c),
            KeyCode::Enter => label.push_str("enter"),
            KeyCode::Esc => label.push_str("esc"),
            KeyCode::Tab => label.push_str("tab"),
            KeyCode::BackTab => label.push_str("⇧tab"),
            KeyCode::Backspace => label.push_str("backspace"),
            KeyCode::Delete => label.push_str("delete"),
            KeyCode::Insert => label.push_str("insert"),
            KeyCode::Up => label.push('↑'),
            KeyCode::Down => label.push('↓'),
            KeyCode::Left => label.push('←'),
            KeyCode::Right => label.push('→'),
            KeyCode::Home => label.push_str("home"),
            KeyCode::End => label.push_str("end"),
            KeyCode::PageUp => label.push_str("pgup"),
            KeyCode::PageDown => label.push_str("pgdn"),
            KeyCode::F(n) => label.push_str(&format!("f{n}")),
            _ => label.push('?'),
        }
        label
    }
}

/// The keys of every action of one view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap<A> {
    /// In `A::TABLE`'s order, which is the order a shared key tries them in.
    bindings: Vec<(A, Vec<Chord>)>,
}

impl<A: Action> Default for Keymap<A> {
    fn default() -> Self {
        let bindings = A::TABLE
            .iter()
            .map(|(_, action, keys)| {
                let chords = keys
                    .iter()
                    .map(|key| match Chord::parse(key) {
                        Ok(chord) => chord,
                        Err(why) => panic!("a default key of keys.{}: {why}", A::VIEW),
                    })
                    .collect();
                (*action, chords)
            })
            .collect();
        Keymap { bindings }
    }
}

impl<A: Action> Keymap<A> {
    /// The defaults, with the actions `overrides` names bound to its keys
    /// instead. A key taken that way leaves the defaults of other actions, so
    /// it does one thing; two actions named with the same key are refused.
    pub fn with(overrides: &BTreeMap<String, Keys>) -> Result<Self, String> {
        let view = A::VIEW;
        let mut chosen: Vec<(A, &str, Vec<Chord>)> = Vec::new();
        for (name, keys) in overrides {
            let Some((_, action, _)) = A::TABLE.iter().find(|(known, _, _)| known == name) else {
                let names: Vec<&str> = A::TABLE.iter().map(|(name, _, _)| *name).collect();
                return Err(format!(
                    "keys.{view} has no action `{name}`. The actions are: {}.",
                    names.join(", ")
                ));
            };
            let chords = keys
                .list()
                .iter()
                .map(|key| Chord::parse(key).map_err(|why| format!("keys.{view}.{name}: {why}")))
                .collect::<Result<Vec<_>, _>>()?;
            chosen.push((*action, name, chords));
        }
        for (i, (_, name, chords)) in chosen.iter().enumerate() {
            for (_, other, theirs) in &chosen[i + 1..] {
                if let Some(chord) = chords.iter().find(|chord| theirs.contains(chord)) {
                    return Err(format!(
                        "keys.{view}: `{}` is bound to both `{name}` and `{other}`. Give it to one of them.",
                        chord.label()
                    ));
                }
            }
        }
        let mut keymap = Keymap::default();
        for (action, chords) in &mut keymap.bindings {
            match chosen.iter().find(|(mine, _, _)| mine == action) {
                Some((_, _, mine)) => *chords = mine.clone(),
                None => chosen
                    .iter()
                    .for_each(|(_, _, taken)| chords.retain(|chord| !taken.contains(chord))),
            }
        }
        Ok(keymap)
    }

    /// The actions `key` is bound to, to be tried in order. A binding with
    /// exactly the modifiers held comes first, so `ctrl+d` finds its own
    /// action before `d`'s; then those that ask for fewer of them, so `j` also
    /// answers to `ctrl+j` when nothing else takes it.
    pub fn actions(&self, key: &KeyEvent) -> Vec<A> {
        let pressed = Chord::of(key);
        let held = |exact: bool| {
            self.bindings.iter().filter_map(move |(action, chords)| {
                chords
                    .iter()
                    .any(|chord| {
                        chord.code == pressed.code
                            && if exact {
                                chord.mods == pressed.mods
                            } else {
                                chord.mods != pressed.mods && pressed.mods.contains(chord.mods)
                            }
                    })
                    .then_some(*action)
            })
        };
        held(true).chain(held(false)).collect()
    }

    /// Whether `key` is bound to `action`.
    pub fn is(&self, key: &KeyEvent, action: A) -> bool {
        self.actions(key).contains(&action)
    }

    /// The keys of `action` for a help screen, such as `j/↓`; empty when unbound.
    pub fn label(&self, action: A) -> String {
        self.bindings
            .iter()
            .find(|(mine, _)| *mine == action)
            .map(|(_, chords)| {
                chords
                    .iter()
                    .map(|chord| chord.label())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .unwrap_or_default()
    }

    /// The keys of several actions, such as `h/←  l/→`, skipping unbound ones.
    pub fn labels(&self, actions: &[A]) -> String {
        actions
            .iter()
            .map(|action| self.label(*action))
            .filter(|label| !label.is_empty())
            .collect::<Vec<_>>()
            .join("  ")
    }
}

/// One key or several, as `config.toml` writes them: `quit = "q"` or
/// `quit = ["q", "ctrl+c"]`. An empty list unbinds the action.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Keys {
    One(String),
    Many(Vec<String>),
}

impl Keys {
    fn list(&self) -> &[String] {
        match self {
            Keys::One(key) => std::slice::from_ref(key),
            Keys::Many(keys) => keys,
        }
    }
}

impl<'de, A: Action> Deserialize<'de> for Keymap<A> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let overrides = BTreeMap::<String, Keys>::deserialize(deserializer)?;
        Keymap::with(&overrides).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Test {
        Down,
        HalfDown,
        Next,
        Turn,
        Quit,
    }

    impl Action for Test {
        const VIEW: &'static str = "test";
        const TABLE: &'static [(&'static str, Self, &'static [&'static str])] = &[
            ("down", Test::Down, &["j", "down"]),
            ("half_down", Test::HalfDown, &["ctrl+d"]),
            ("next", Test::Next, &["n"]),
            ("turn", Test::Turn, &["n", "d"]),
            ("quit", Test::Quit, &["q"]),
        ];
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn char(c: char) -> KeyEvent {
        press(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn overrides(text: &str) -> Result<Keymap<Test>, String> {
        let table: BTreeMap<String, Keys> = toml::from_str(text).map_err(|e| e.to_string())?;
        Keymap::with(&table)
    }

    #[test]
    fn parses_keys_with_modifiers() {
        let parse = |text| Chord::parse(text).unwrap();
        assert_eq!(
            parse("ctrl+d"),
            Chord::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
        );
        assert_eq!(parse("shift+g"), parse("G"));
        assert_eq!(parse("shift+tab"), parse("backtab"));
        assert_eq!(
            parse("+"),
            Chord::new(KeyCode::Char('+'), KeyModifiers::NONE)
        );
        assert_eq!(
            parse("ctrl++"),
            Chord::new(KeyCode::Char('+'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            parse("Alt+Enter"),
            Chord::new(KeyCode::Enter, KeyModifiers::ALT)
        );
        assert_eq!(parse("space"), parse(" "));
        assert_eq!(parse("f5"), Chord::new(KeyCode::F(5), KeyModifiers::NONE));
        assert!(Chord::parse("hyper+x").is_err());
        assert!(Chord::parse("f13").is_err());
        assert!(Chord::parse("").is_err());
        assert!(Chord::parse("ctrl+").is_err());
    }

    #[test]
    fn labels_keys_as_the_help_screens_do() {
        for (text, label) in [
            ("ctrl+d", "^d"),
            ("G", "G"),
            ("space", "space"),
            ("alt+enter", "⌥enter"),
            ("shift+up", "⇧↑"),
            ("f3", "f3"),
            ("pagedown", "pgdn"),
        ] {
            assert_eq!(Chord::parse(text).unwrap().label(), label, "{text}");
        }
    }

    #[test]
    fn exact_modifiers_win_over_fewer() {
        let keys = Keymap::<Test>::default();
        assert_eq!(
            keys.actions(&press(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            vec![Test::HalfDown, Test::Turn]
        );
        assert_eq!(keys.actions(&char('d')), vec![Test::Turn]);
        assert_eq!(
            keys.actions(&press(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            vec![Test::Down]
        );
        // Shift is the letter's case, however the terminal reports it.
        assert_eq!(
            keys.actions(&press(KeyCode::Char('Q'), KeyModifiers::SHIFT)),
            Vec::<Test>::new()
        );
    }

    #[test]
    fn a_shared_key_tries_actions_in_table_order() {
        let keys = Keymap::<Test>::default();
        assert_eq!(keys.actions(&char('n')), vec![Test::Next, Test::Turn]);
    }

    #[test]
    fn an_override_replaces_that_actions_keys_only() {
        let keys = overrides("down = [\"e\", \"down\"]").unwrap();
        assert!(keys.actions(&char('j')).is_empty());
        assert_eq!(keys.actions(&char('e')), vec![Test::Down]);
        assert_eq!(keys.actions(&char('q')), vec![Test::Quit]);
        assert_eq!(keys.label(Test::Down), "e/↓");
    }

    #[test]
    fn a_key_taken_by_an_override_leaves_other_defaults() {
        let keys = overrides("quit = \"n\"").unwrap();
        assert_eq!(keys.actions(&char('n')), vec![Test::Quit]);
        assert!(keys.actions(&char('q')).is_empty());
        assert_eq!(keys.label(Test::Turn), "d");
    }

    #[test]
    fn an_empty_list_unbinds() {
        let keys = overrides("quit = []").unwrap();
        assert!(keys.actions(&char('q')).is_empty());
        assert_eq!(keys.labels(&[Test::Quit, Test::Next]), "n");
    }

    #[test]
    fn refuses_unknown_actions_bad_keys_and_clashes() {
        let unknown = overrides("jump = \"x\"").unwrap_err();
        assert!(
            unknown.contains("no action `jump`") && unknown.contains("half_down"),
            "{unknown}"
        );
        let bad = overrides("quit = \"ctl+q\"").unwrap_err();
        assert!(
            bad.contains("keys.test.quit") && bad.contains("`ctl`"),
            "{bad}"
        );
        let clash = overrides("quit = \"x\"\nnext = [\"y\", \"x\"]").unwrap_err();
        assert!(clash.contains("`x` is bound to both"), "{clash}");
    }
}
