//! The two-row "tiny" block font OpenTUI draws large headings in (from cfonts).

const GLYPHS: &[(char, [&str; 2])] = &[
    ('0', ["▞█▚", "▚█▞"]),
    ('1', ["▄█", " █"]),
    ('2', ["▀█", "█▄"]),
    ('3', ["▀▀█", "▄██"]),
    ('4', ["█ █", "▀▀█"]),
    ('5', ["█▀", "▄█"]),
    ('6', ["█▄▄", "█▄█"]),
    ('7', ["▀▀█", "  █"]),
    ('8', ["███", "█▄█"]),
    ('9', ["█▀█", "▀▀█"]),
    ('A', ["▄▀█", "█▀█"]),
    ('B', ["█▄▄", "█▄█"]),
    ('C', ["█▀▀", "█▄▄"]),
    ('D', ["█▀▄", "█▄▀"]),
    ('E', ["█▀▀", "██▄"]),
    ('F', ["█▀▀", "█▀ "]),
    ('G', ["█▀▀", "█▄█"]),
    ('H', ["█ █", "█▀█"]),
    ('I', ["█", "█"]),
    ('J', ["  █", "█▄█"]),
    ('K', ["█▄▀", "█ █"]),
    ('L', ["█  ", "█▄▄"]),
    ('M', ["█▀▄▀█", "█ ▀ █"]),
    ('N', ["█▄ █", "█ ▀█"]),
    ('O', ["█▀█", "█▄█"]),
    ('P', ["█▀█", "█▀▀"]),
    ('Q', ["█▀█", "▀▀█"]),
    ('R', ["█▀█", "█▀▄"]),
    ('S', ["█▀▀", "▄▄█"]),
    ('T', ["▀█▀", " █ "]),
    ('U', ["█ █", "█▄█"]),
    ('V', ["█ █", "▀▄▀"]),
    ('W', ["█ █ █", "▀▄▀▄▀"]),
    ('X', ["▀▄▀", "█ █"]),
    ('Y', ["█▄█", " █ "]),
    ('Z', ["▀█", "█▄"]),
    ('!', ["█", "▄"]),
    ('?', ["▀█", " ▄"]),
    ('.', [" ", "▄"]),
    ('+', ["▄█▄", " ▀ "]),
    ('-', ["▄▄", "  "]),
    ('_', ["  ", "▄▄"]),
    ('=', ["▀▀", "▀▀"]),
    ('@', ["▛█▜", "▙▟▃"]),
    ('#', ["▟▄▙", "▜▀▛"]),
    ('$', ["▖█▗", "▘█▝"]),
    ('%', ["▀ ▄▀", "▄▀ ▄"]),
    ('&', ["▄▄█", "█▄█"]),
    ('(', ["▄▀", "▀▄"]),
    (')', ["▀▄", "▄▀"]),
    ('/', ["  ▄▀", "▄▀  "]),
    (':', ["▀", "▄"]),
    (';', ["  ", "▄▀"]),
    (',', [" ", "█"]),
    ('\'', ["▀", " "]),
    ('"', ["▛ ▜", "   "]),
    (' ', [" ", " "]),
];

fn glyph(c: char) -> Option<[&'static str; 2]> {
    let upper = c.to_uppercase().next().unwrap_or(c);
    GLYPHS.iter().find(|(g, _)| *g == upper).map(|(_, rows)| *rows)
}

/// Width in columns of `text` set in the font. Letters are a column apart;
/// a character the font lacks takes a space's width.
pub fn width(text: &str) -> usize {
    render(text)[0].chars().count()
}

/// The two rows of `text` set in the font.
pub fn render(text: &str) -> [String; 2] {
    let mut rows = [String::new(), String::new()];
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        match glyph(c) {
            Some([top, bottom]) => {
                rows[0].push_str(top);
                rows[1].push_str(bottom);
                if i + 1 < chars.len() {
                    rows[0].push(' ');
                    rows[1].push(' ');
                }
            }
            None => {
                rows[0].push(' ');
                rows[1].push(' ');
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_uppercased_and_spaced() {
        let [top, bottom] = render("hi");
        assert_eq!(top, "█ █ █");
        assert_eq!(bottom, "█▀█ █");
        assert_eq!(width("hi"), 5);
    }
}
