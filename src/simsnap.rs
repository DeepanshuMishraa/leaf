//! `leaf sim snapshot`: the simulator's screen as a short outline an agent
//! reads instead of a screenshot, with a ref (`@e3`) on each element to tap.
//!
//! The pane (or `leaf-sim-host --drive`) sends the frontmost app's
//! accessibility tree; this module drops what an agent can't use, names
//! each element once, and keeps the refs of elements that are still on
//! screen from one snapshot to the next, so `--diff` shows only what moved
//! and a ref stays good while its element is there.

use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffTag, capture_diff_slices};

/// Longest label shown, in characters; the rest is cut with an ellipsis.
const LABEL_LIMIT: usize = 80;

/// Roles an agent reads but doesn't act on; `-i` leaves them out.
const PASSIVE: [&str; 10] = [
    "StaticText",
    "Heading",
    "Image",
    "Group",
    "ScrollArea",
    "Table",
    "List",
    "Outline",
    "Unknown",
    "Window",
];

/// One element as the helper sends it. Frames are screenshot pixels.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Element {
    pub depth: usize,
    pub role: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub id: String,
    /// x, y, width, height.
    pub frame: [i64; 4],
    #[serde(default)]
    pub disabled: bool,
}

/// The frontmost app's elements, depth first, the app itself first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tree {
    pub width: i64,
    pub height: i64,
    pub elements: Vec<Element>,
}

/// What names an element across snapshots: its role, label and id, and
/// which of the elements sharing those it is, in order, out of how many.
/// When one of several identical elements comes or goes, the count
/// changes and all of their refs retire, so a survivor can't take over a
/// removed sibling's ref. Its value isn't part of it, so a field keeps its
/// ref while it's typed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub role: String,
    pub label: String,
    pub id: String,
    pub nth: usize,
    #[serde(default)]
    pub of: usize,
}

/// One ref handed out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub number: u32,
    pub key: Key,
    /// Where it was, to tell identical elements apart if they move.
    #[serde(default)]
    pub frame: [i64; 4],
}

/// The last snapshot of a device: its refs, and its text for `--diff`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The number the next new element gets.
    pub next: u32,
    pub entries: Vec<Entry>,
    pub text: String,
    /// A fingerprint of the whole tree, whatever the detail shown, to tell
    /// whether the screen changed.
    #[serde(default)]
    pub screen: u64,
    /// The app in front; empty on the home screen. Refs belong to it.
    #[serde(default)]
    pub app: String,
}

/// Why a ref from the last snapshot can't be acted on now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    /// The last snapshot handed out no such ref.
    Unknown,
    /// Another app is in front than the one the snapshot read.
    OtherApp { then: String, now: String },
    /// Its element isn't on screen.
    Gone(Key),
    /// It's one of several identical elements, and they moved, so which
    /// one it was can't be told.
    Moved(Key),
}

/// Whether a snapshot shows every element or only those to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Full,
    Interactive,
}

/// A ref as typed: `@e3` or `e3`.
pub fn parse_ref(text: &str) -> Option<u32> {
    let digits = text.strip_prefix('@').unwrap_or(text).strip_prefix('e')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

impl Element {
    fn visible(&self, width: i64, height: i64) -> bool {
        let [x, y, w, h] = self.frame;
        w > 0 && h > 0 && x < width && y < height && x + w > 0 && y + h > 0
    }

    /// A container with nothing to say of its own; its children stand in for it.
    fn hollow(&self) -> bool {
        matches!(self.role.as_str(), "Group" | "Unknown" | "ScrollArea" | "")
            && self.label.is_empty()
            && self.value.is_empty()
    }

    fn passive(&self) -> bool {
        PASSIVE.contains(&self.role.as_str())
    }

    /// Where a tap on the element goes, on a `width`×`height` screen: the
    /// middle of its visible part. A switch's element is its whole row,
    /// and a tap on the row's label doesn't flip it, so a switch is
    /// tapped at the row's right end, where the switch itself sits.
    pub fn tap_point(&self, width: i64, height: i64) -> (i64, i64) {
        let h = self.frame[3];
        let [left, top, w, visible_h] = self.visible_frame(width, height);
        let (right, bottom) = (left + w, top + visible_h);
        let middle = (top + bottom) / 2;
        if self.role == "CheckBox" && right - left > 2 * h {
            return (right - h * 9 / 10, middle);
        }
        ((left + right) / 2, middle)
    }

    /// The part of the element on a `width`×`height` screen, as x, y,
    /// width and height.
    pub fn visible_frame(&self, width: i64, height: i64) -> [i64; 4] {
        let [x, y, w, h] = self.frame;
        let (left, top) = (x.max(0), y.max(0));
        let (right, bottom) = ((x + w).min(width), (y + h).min(height));
        [left, top, (right - left).max(0), (bottom - top).max(0)]
    }

    fn line(&self, number: u32) -> String {
        let role = match self.role.as_str() {
            "StaticText" => "text".to_owned(),
            role => role.to_lowercase(),
        };
        let mut line = format!("@e{number} {role}");
        if !self.label.is_empty() {
            line.push_str(&format!(" \"{}\"", clip(&self.label)));
        } else if !self.id.is_empty() {
            line.push_str(&format!(" #{}", clip(&self.id)));
        }
        if !self.value.is_empty() && self.value != self.label {
            line.push_str(&format!(" = \"{}\"", clip(&self.value)));
        }
        if self.disabled {
            line.push_str(" (disabled)");
        }
        line
    }
}

/// `text` on one line, at most `LABEL_LIMIT` characters.
fn clip(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    match flat.char_indices().nth(LABEL_LIMIT) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat.to_owned(),
    }
}

impl Tree {
    /// The app's own element, when the tree starts with one.
    fn app(&self) -> Option<&Element> {
        self.elements
            .first()
            .filter(|element| element.role == "Application")
    }

    /// The frontmost app's name; none on the home screen.
    pub fn app_name(&self) -> Option<&str> {
        self.app()
            .map(|app| app.label.trim())
            .filter(|name| !name.is_empty())
    }

    /// Every element but the app, with its key.
    pub fn keyed(&self) -> Vec<(Key, &Element)> {
        let skip = usize::from(self.app().is_some());
        let elements: Vec<&Element> = self.elements.iter().skip(skip).collect();
        let same =
            |a: &Element, b: &Element| a.role == b.role && a.label == b.label && a.id == b.id;
        elements
            .iter()
            .enumerate()
            .map(|(index, element)| {
                let key = Key {
                    role: element.role.clone(),
                    label: element.label.clone(),
                    id: element.id.clone(),
                    nth: elements[..index]
                        .iter()
                        .filter(|e| same(e, element))
                        .count(),
                    of: elements.iter().filter(|e| same(e, element)).count(),
                };
                (key, *element)
            })
            .collect()
    }

    /// The element `key` names, if it's on screen now.
    pub fn find(&self, key: &Key) -> Option<&Element> {
        self.keyed()
            .into_iter()
            .find(|(found, _)| found == key)
            .map(|(_, element)| element)
            .filter(|element| element.visible(self.width, self.height))
    }

    /// The element `@e<number>` from `snapshot` names, on this screen.
    pub fn resolve(&self, snapshot: &Snapshot, number: u32) -> Result<&Element, Unresolved> {
        let entry = snapshot.entry(number).ok_or(Unresolved::Unknown)?;
        let now = self.app_name().unwrap_or_default();
        if now != snapshot.app {
            return Err(Unresolved::OtherApp {
                then: snapshot.app.clone(),
                now: now.to_owned(),
            });
        }
        let element = self
            .find(&entry.key)
            .ok_or_else(|| Unresolved::Gone(entry.key.clone()))?;
        // Order alone can't tell identical elements apart once they've
        // moved (rows reordered, or the list scrolled): refuse to guess.
        if entry.key.of > 1 && element.frame != entry.frame {
            return Err(Unresolved::Moved(entry.key.clone()));
        }
        Ok(element)
    }

    /// The lines of `snapshot` for elements whose label, value or id
    /// contains `wanted`, ignoring case, in full rather than as shown.
    pub fn matching(&self, snapshot: &Snapshot, wanted: &str) -> Vec<String> {
        let wanted = wanted.to_lowercase();
        self.keyed()
            .into_iter()
            .filter(|(_, element)| {
                element.visible(self.width, self.height)
                    && !element.hollow()
                    && [&element.label, &element.value, &element.id]
                        .iter()
                        .any(|text| text.to_lowercase().contains(&wanted))
            })
            .filter_map(|(key, element)| {
                let entry = snapshot.entries.iter().find(|entry| entry.key == key)?;
                Some(element.line(entry.number))
            })
            .collect()
    }

    /// The outline, keeping `previous`'s refs for elements still here.
    pub fn snapshot(&self, previous: &Snapshot, detail: Detail) -> Snapshot {
        let mut next = previous.next.max(1);
        let app = self.app_name().unwrap_or_default().to_owned();
        // Refs belong to one app: another app's identical "Delete" is a
        // different button.
        let kept: &[Entry] = if previous.app == app {
            &previous.entries
        } else {
            &[]
        };
        let mut entries = Vec::new();
        let mut lines = Vec::new();
        let size = format!("{}×{}px", self.width, self.height);
        lines.push(match self.app_name() {
            Some(name) => format!("{} {size}", clip(name)),
            None => size,
        });
        // Depths of the shown elements the current one sits inside.
        let mut shown: Vec<usize> = Vec::new();
        for (key, element) in self.keyed() {
            while shown.last().is_some_and(|&depth| depth >= element.depth) {
                shown.pop();
            }
            if !element.visible(self.width, self.height) || element.hollow() {
                continue;
            }
            // Every element gets its ref, printed or not, so a full
            // snapshot after an interactive one keeps the same numbers.
            let number = match kept.iter().find(|entry| entry.key == key) {
                Some(entry) => entry.number,
                None => {
                    next += 1;
                    next - 1
                }
            };
            entries.push(Entry {
                number,
                key,
                frame: element.frame,
            });
            if detail == Detail::Interactive && element.passive() {
                continue;
            }
            let indent = match detail {
                Detail::Full => "  ".repeat(shown.len()),
                Detail::Interactive => String::new(),
            };
            lines.push(format!("{indent}{}", element.line(number)));
            shown.push(element.depth);
        }
        // Refs of elements gone from this snapshot stay reserved, so an
        // old ref can't come to mean something else.
        let mut text = lines.join("\n");
        text.push('\n');
        Snapshot {
            next,
            entries,
            text,
            screen: crate::plugin::fnv1a(&format!("{:?}", self.elements)),
            app,
        }
    }
}

impl Snapshot {
    pub fn entry(&self, number: u32) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.number == number)
    }

    /// The lines that changed since `before`, `-` for gone and `+` for new;
    /// the whole snapshot when that's shorter, as on a new screen.
    pub fn diff(&self, before: &Snapshot) -> String {
        let old: Vec<&str> = before.text.lines().collect();
        let new: Vec<&str> = self.text.lines().collect();
        let mut out = String::new();
        for op in capture_diff_slices(Algorithm::Myers, &old, &new) {
            let (tag, gone, added) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                continue;
            }
            for line in &old[gone] {
                out.push_str(&format!("- {line}\n"));
            }
            for line in &new[added] {
                out.push_str(&format!("+ {line}\n"));
            }
        }
        if out.is_empty() {
            out.push_str("no changes\n");
        } else if out.len() > self.text.len() {
            return self.text.clone();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(depth: usize, role: &str, label: &str, frame: [i64; 4]) -> Element {
        Element {
            depth,
            role: role.into(),
            label: label.into(),
            value: String::new(),
            id: String::new(),
            frame,
            disabled: false,
        }
    }

    fn settings() -> Tree {
        Tree {
            width: 1206,
            height: 2622,
            elements: vec![
                element(0, "Application", "Settings", [0, 0, 1206, 2622]),
                element(1, "Heading", "Settings", [48, 359, 399, 122]),
                Element {
                    id: "com.apple.settings.sidebar.collectionView".into(),
                    ..element(1, "Group", "", [0, 0, 1206, 2622])
                },
                element(2, "Button", "General", [48, 1141, 1110, 156]),
                element(2, "Button", "Camera", [48, 1765, 1110, 156]),
                Element {
                    value: "Search".into(),
                    ..element(1, "TextField", "", [84, 2424, 1038, 84])
                },
            ],
        }
    }

    #[test]
    fn refs_parse_with_or_without_the_at() {
        assert_eq!(parse_ref("@e3"), Some(3));
        assert_eq!(parse_ref("e12"), Some(12));
        assert_eq!(parse_ref("@e"), None);
        assert_eq!(parse_ref("3"), None);
        assert_eq!(parse_ref("@e3x"), None);
    }

    #[test]
    fn the_outline_drops_hollow_groups_and_names_the_app() {
        let snapshot = settings().snapshot(&Snapshot::default(), Detail::Full);
        assert_eq!(
            snapshot.text,
            "Settings 1206×2622px\n\
             @e1 heading \"Settings\"\n\
             @e2 button \"General\"\n\
             @e3 button \"Camera\"\n\
             @e4 textfield = \"Search\"\n"
        );
    }

    #[test]
    fn children_of_a_shown_element_are_indented() {
        let tree = Tree {
            width: 100,
            height: 100,
            elements: vec![
                element(0, "Application", "App", [0, 0, 100, 100]),
                element(1, "NavigationBar", "Title", [0, 0, 100, 10]),
                element(2, "Button", "Back", [0, 0, 10, 10]),
                element(1, "Button", "Go", [0, 50, 10, 10]),
            ],
        };
        let text = tree.snapshot(&Snapshot::default(), Detail::Full).text;
        assert_eq!(
            text,
            "App 100×100px\n@e1 navigationbar \"Title\"\n  @e2 button \"Back\"\n@e3 button \"Go\"\n"
        );
    }

    #[test]
    fn interactive_detail_keeps_only_what_can_be_acted_on() {
        let snapshot = settings().snapshot(&Snapshot::default(), Detail::Interactive);
        assert_eq!(
            snapshot.text,
            "Settings 1206×2622px\n@e2 button \"General\"\n@e3 button \"Camera\"\n@e4 textfield = \"Search\"\n"
        );
    }

    #[test]
    fn refs_hold_when_switching_between_interactive_and_full() {
        let interactive = settings().snapshot(&Snapshot::default(), Detail::Interactive);
        let full = settings().snapshot(&interactive, Detail::Full);
        // The heading -i left out kept the ref it was given.
        assert_eq!(
            full.text,
            settings().snapshot(&Snapshot::default(), Detail::Full).text
        );
    }

    #[test]
    fn refs_survive_the_next_snapshot_and_new_elements_get_new_numbers() {
        let first = settings().snapshot(&Snapshot::default(), Detail::Full);
        let mut tree = settings();
        tree.elements.remove(4); // Camera scrolled away
        tree.elements
            .insert(3, element(2, "Button", "About", [48, 900, 1110, 156]));
        let second = tree.snapshot(&first, Detail::Full);
        assert!(second.text.contains("@e2 button \"General\""));
        assert!(second.text.contains("@e5 button \"About\""));
        assert!(!second.text.contains("Camera"));
        // e3 (Camera) stays retired.
        assert_eq!(second.next, 6);
        assert_eq!(
            second.diff(&first),
            "+ @e5 button \"About\"\n- @e3 button \"Camera\"\n"
        );
        assert_eq!(second.diff(&second), "no changes\n");
    }

    #[test]
    fn a_new_screen_diffs_as_itself_and_the_home_screen_has_no_name() {
        let first = settings().snapshot(&Snapshot::default(), Detail::Full);
        let home = Tree {
            width: 1206,
            height: 2622,
            elements: vec![
                element(0, "Application", "", [0, 0, 1206, 2622]),
                element(1, "Button", "Maps", [72, 264, 505, 573]),
            ],
        };
        let second = home.snapshot(&first, Detail::Full);
        assert_eq!(second.text, "1206×2622px\n@e5 button \"Maps\"\n");
        assert_eq!(second.diff(&first), second.text);
    }

    #[test]
    fn a_typed_value_keeps_the_fields_ref() {
        let first = settings().snapshot(&Snapshot::default(), Detail::Full);
        let mut tree = settings();
        tree.elements[5].value = "wi".into();
        let second = tree.snapshot(&first, Detail::Full);
        assert!(second.text.contains("@e4 textfield = \"wi\""));
        assert_ne!(first.screen, second.screen);
    }

    #[test]
    fn the_screen_fingerprint_ignores_the_detail_shown() {
        let full = settings().snapshot(&Snapshot::default(), Detail::Full);
        let interactive = settings().snapshot(&full, Detail::Interactive);
        assert_ne!(full.text, interactive.text);
        assert_eq!(full.screen, interactive.screen);
    }

    #[test]
    fn elements_with_the_same_name_are_told_apart_by_order() {
        let tree = Tree {
            width: 100,
            height: 100,
            elements: vec![
                element(0, "Button", "Delete", [0, 0, 10, 10]),
                element(0, "Button", "Delete", [0, 20, 10, 10]),
            ],
        };
        let snapshot = tree.snapshot(&Snapshot::default(), Detail::Full);
        let second = snapshot.entry(2).map(|entry| entry.key.clone());
        let found = second.and_then(|key| tree.find(&key).cloned());
        assert_eq!(found.map(|element| element.frame), Some([0, 20, 10, 10]));
    }

    #[test]
    fn a_removed_duplicate_retires_its_siblings_refs() {
        let two = Tree {
            width: 100,
            height: 100,
            elements: vec![
                element(0, "Button", "Delete", [0, 0, 10, 10]),
                element(0, "Button", "Delete", [0, 20, 10, 10]),
            ],
        };
        let first = two.snapshot(&Snapshot::default(), Detail::Full);
        let mut one = two.clone();
        one.elements.remove(0);
        // @e1 named the removed button; the survivor must not take it over,
        // and @e2 no longer finds anything.
        let second = one.snapshot(&first, Detail::Full);
        assert_eq!(second.text, "100×100px\n@e3 button \"Delete\"\n");
        let old = first.entry(2).map(|entry| entry.key.clone());
        assert_eq!(old.and_then(|key| one.find(&key).cloned()), None);
    }

    #[test]
    fn offscreen_elements_are_left_out_and_cant_be_found() {
        let mut tree = settings();
        tree.elements
            .push(element(2, "Button", "Below", [48, 2700, 1110, 156]));
        let snapshot = tree.snapshot(&Snapshot::default(), Detail::Full);
        assert!(!snapshot.text.contains("Below"));
        let key = Key {
            role: "Button".into(),
            label: "Below".into(),
            id: String::new(),
            nth: 0,
            of: 1,
        };
        assert_eq!(tree.find(&key), None);
    }

    #[test]
    fn taps_land_mid_screen_part_or_on_a_switch() {
        let partly = element(1, "Button", "Half", [-100, 2500, 300, 300]);
        // On screen: x 0 to 200, y 2500 to 2622.
        assert_eq!(partly.tap_point(1206, 2622), (100, 2561));
        // Settings' Haptic Feedback row; its switch is at the right end.
        let switch = element(2, "CheckBox", "Haptic Feedback", [108, 1444, 990, 84]);
        assert_eq!(switch.tap_point(1206, 2622), (1023, 1486));
    }

    #[test]
    fn long_labels_are_cut_and_disabled_ids_show() {
        let long = "a".repeat(100);
        let line = Element {
            id: "save".into(),
            disabled: true,
            ..element(0, "Button", "", [0, 0, 1, 1])
        }
        .line(7);
        assert_eq!(line, "@e7 button #save (disabled)");
        assert_eq!(clip(&long).chars().count(), LABEL_LIMIT + 1);
        assert_eq!(clip("two\nlines"), "two lines");
    }

    #[test]
    fn the_helpers_json_parses() {
        let json = r#"{"width":1206,"height":2622,"elements":[
            {"depth":0,"role":"Application","label":"Settings","frame":[0,0,1206,2622]},
            {"depth":1,"role":"Button","id":"x","disabled":true,"frame":[1,2,3,4]}]}"#;
        let tree: Tree = serde_json::from_str(json).expect("parses");
        assert_eq!(tree.elements.len(), 2);
        assert!(tree.elements[1].disabled);
    }

    fn two_deletes(app: &str) -> Tree {
        Tree {
            width: 100,
            height: 100,
            elements: vec![
                element(0, "Application", app, [0, 0, 100, 100]),
                element(1, "Button", "Delete", [0, 0, 10, 10]),
                element(1, "Button", "Delete", [0, 20, 10, 10]),
            ],
        }
    }

    #[test]
    fn refs_resolve_in_their_own_app_only() {
        let notes = two_deletes("Notes");
        let snapshot = notes.snapshot(&Snapshot::default(), Detail::Full);
        assert_eq!(
            notes.resolve(&snapshot, 2).map(|element| element.frame),
            Ok([0, 20, 10, 10])
        );
        assert_eq!(notes.resolve(&snapshot, 9), Err(Unresolved::Unknown));
        let mail = two_deletes("Mail");
        assert_eq!(
            mail.resolve(&snapshot, 2),
            Err(Unresolved::OtherApp {
                then: "Notes".into(),
                now: "Mail".into()
            })
        );
        // Mail's buttons get refs of their own, not Notes' ones.
        let fresh = mail.snapshot(&snapshot, Detail::Full);
        assert_eq!(
            fresh.text,
            "Mail 100×100px\n@e3 button \"Delete\"\n@e4 button \"Delete\"\n"
        );
    }

    #[test]
    fn moved_duplicates_are_refused_not_guessed() {
        let notes = two_deletes("Notes");
        let snapshot = notes.snapshot(&Snapshot::default(), Detail::Full);
        let mut scrolled = notes.clone();
        scrolled.elements[1].frame = [0, 40, 10, 10];
        scrolled.elements[2].frame = [0, 60, 10, 10];
        assert!(matches!(
            scrolled.resolve(&snapshot, 1),
            Err(Unresolved::Moved(_))
        ));
        // A unique element may move freely.
        let mut settings = settings();
        let first = settings.snapshot(&Snapshot::default(), Detail::Full);
        settings.elements[3].frame[1] += 300;
        assert!(settings.resolve(&first, 2).is_ok());
    }

    #[test]
    fn wait_matches_whole_labels_not_shown_lines() {
        let mut tree = settings();
        let long = format!("{}needle", "a".repeat(100));
        tree.elements
            .push(element(1, "StaticText", &long, [48, 2000, 1110, 60]));
        let snapshot = tree.snapshot(&Snapshot::default(), Detail::Full);
        // Past the clip, and not a role name.
        assert_eq!(tree.matching(&snapshot, "NEEDLE").len(), 1);
        assert!(tree.matching(&snapshot, "button").is_empty());
        assert_eq!(
            tree.matching(&snapshot, "gener"),
            vec!["@e2 button \"General\"".to_owned()]
        );
    }
}
