//! `leaf sponsors`: a full-screen view of everyone on the
//! site's sponsor list (`/api/sponsors`, biggest backers first): the newest
//! sponsor in a spotlight of their own, everyone else in a centred grid of
//! round faces. Faces are Kitty graphics where the terminal draws them and
//! half-block pixels elsewhere. Piped, it prints a plain list instead.

use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use image::{Rgba, RgbaImage, imageops::FilterType};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};
use ratatui_image::picker::{Picker, ProtocolType};
use serde::Deserialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::kitty::{self, Kitty};

/// The newest sponsor, shown in the spotlight: their GitHub login, or their
/// name if they have none. Updated by hand when someone new joins.
const NEWEST: &str = "ethanolivertroy";

const LIST_URL: &str = "https://leaf.dipxsy.app/api/sponsors";
const SPONSOR_URL: &str = "https://leaf.dipxsy.app/sponsor";
const USER_AGENT: &str = concat!("leaf/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(8);
const MAX_LIST_BYTES: u64 = 1 << 20;
const MAX_AVATAR_BYTES: u64 = 4 << 20;

/// Cells per grid card across, and a card's height: face, gap, name, login.
const CARD: u16 = 18;
const CARD_HEIGHT: u16 = GRID_FACE.rows + 3;
const ROW_GAP: u16 = 1;
/// The spotlight's box: border and padding around face, gap, name, login.
const SPOT_WIDTH: u16 = 40;
const SPOT_HEIGHT: u16 = SPOT_FACE.rows + 7;

#[derive(Clone, Copy)]
struct Size {
    cols: u16,
    rows: u16,
}

const GRID_FACE: Size = Size { cols: 12, rows: 6 };
const SPOT_FACE: Size = Size { cols: 20, rows: 10 };

/// Background hues for sponsors without a GitHub face, as on the site.
const HUES: [[u8; 3]; 6] = [
    [0x5b, 0x9a, 0x6e],
    [0x4f, 0x86, 0xc6],
    [0xc0, 0x6c, 0x84],
    [0xd0, 0x8c, 0x3c],
    [0x8a, 0x6f, 0xc4],
    [0x3f, 0x9f, 0xa6],
];

#[derive(Debug, thiserror::Error)]
pub enum SponsorsFailed {
    #[error(
        "couldn't reach the sponsor list at {LIST_URL} ({0}). Check your connection and try again; the list is also on https://leaf.dipxsy.app."
    )]
    Offline(String),
    #[error(
        "the sponsor list at {LIST_URL} came back in a shape leaf doesn't read ({0}). Upgrading leaf may fix it: leaf upgrade."
    )]
    Unreadable(String),
    #[error("the terminal failed ({0}). Pipe it for a plain list: leaf sponsors | cat")]
    Terminal(#[from] io::Error),
}

#[derive(Debug, Deserialize)]
struct Entry {
    name: String,
    #[serde(default)]
    github: Option<String>,
}

/// A sponsor as shown: names and logins come from strangers through the
/// checkout form, so both are cleaned before they reach the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sponsor {
    name: String,
    github: Option<String>,
}

impl Sponsor {
    fn is_newest(&self) -> bool {
        self.github
            .as_deref()
            .is_some_and(|login| login.eq_ignore_ascii_case(NEWEST))
            || self.name.eq_ignore_ascii_case(NEWEST)
    }
}

/// Where a face comes from.
enum Source {
    Picture(RgbaImage),
    Initial { letter: char, color: [u8; 3] },
}

/// A face ready to draw at one size: half-block pixels, a Kitty image when
/// the terminal takes them, and the letter over an initial's disc.
struct Face {
    size: Size,
    pixels: RgbaImage,
    image: Option<kitty::Image>,
    letter: Option<(char, [u8; 3])>,
}

struct Card {
    sponsor: Sponsor,
    face: Face,
}

struct Wall {
    total: usize,
    spotlight: Option<Card>,
    rest: Vec<Card>,
    scroll: u16,
}

/// `leaf sponsors`: shows the sponsors until q.
pub fn run() -> Result<(), SponsorsFailed> {
    let tty = io::stdout().is_terminal() && io::stdin().is_terminal();
    if tty {
        eprint!("Gathering leaf's sponsors…");
    }
    let sponsors = fetch_list();
    let sources = match &sponsors {
        Ok(list) if tty => sources_of(list),
        _ => Vec::new(),
    };
    if tty {
        eprint!("\r\x1b[2K");
    }
    let sponsors = sponsors?;
    if !tty {
        let mut out = io::stdout().lock();
        // A closed pipe (`leaf sponsors | head`) only cuts the list short.
        let _ = print_plain(&mut out, &sponsors);
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = show(&mut terminal, sponsors, sources);
    ratatui::restore();
    result
}

fn show(
    terminal: &mut ratatui::DefaultTerminal,
    sponsors: Vec<Sponsor>,
    sources: Vec<Source>,
) -> Result<(), SponsorsFailed> {
    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::terminal::SetTitle("leaf · sponsors")
    );
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut kitty = (picker.protocol_type() == ProtocolType::Kitty && !picker.tmux_detected())
        .then(Kitty::default);
    let font = picker.font_size();
    let font = (u32::from(font.width.max(1)), u32::from(font.height.max(1)));

    let total = sponsors.len();
    let mut spotlight = None;
    let mut rest = Vec::new();
    for (sponsor, source) in sponsors.into_iter().zip(sources) {
        let newest = spotlight.is_none() && sponsor.is_newest();
        let size = if newest { SPOT_FACE } else { GRID_FACE };
        let face = face(&source, size, kitty.as_mut(), font);
        let card = Card { sponsor, face };
        if newest {
            spotlight = Some(card);
        } else {
            rest.push(card);
        }
    }
    let mut wall = Wall {
        total,
        spotlight,
        rest,
        scroll: 0,
    };

    let result = (|| -> Result<(), SponsorsFailed> {
        loop {
            let mut wanted = Vec::new();
            terminal.draw(|f| wanted = draw(f, &mut wall))?;
            if let Some(kitty) = kitty.as_mut() {
                kitty.flush(&mut io::stdout(), &wanted)?;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char('j') | KeyCode::Down => wall.scroll = wall.scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => wall.scroll = wall.scroll.saturating_sub(1),
                KeyCode::Char(' ') | KeyCode::PageDown => {
                    wall.scroll = wall.scroll.saturating_add(CARD_HEIGHT + ROW_GAP);
                }
                KeyCode::Char('b') | KeyCode::PageUp => {
                    wall.scroll = wall.scroll.saturating_sub(CARD_HEIGHT + ROW_GAP);
                }
                KeyCode::Char('g') | KeyCode::Home => wall.scroll = 0,
                KeyCode::Char('G') | KeyCode::End => wall.scroll = u16::MAX,
                KeyCode::Char('o') => {
                    let _ = crate::prmedia::open(SPONSOR_URL);
                }
                _ => {}
            }
        }
    })();
    if let Some(kitty) = kitty.as_mut() {
        kitty.reset();
        let _ = kitty.flush(&mut io::stdout(), &[]);
    }
    result
}

fn fetch_list() -> Result<Vec<Sponsor>, SponsorsFailed> {
    let body = ureq::get(LIST_URL)
        .config()
        .timeout_global(Some(TIMEOUT))
        .build()
        .header("user-agent", USER_AGENT)
        .call()
        .map_err(|e| SponsorsFailed::Offline(e.to_string()))?
        .into_body()
        .with_config()
        .limit(MAX_LIST_BYTES)
        .read_to_string()
        .map_err(|e| SponsorsFailed::Offline(e.to_string()))?;
    parse(&body)
}

fn parse(body: &str) -> Result<Vec<Sponsor>, SponsorsFailed> {
    let entries: Vec<Entry> =
        serde_json::from_str(body).map_err(|e| SponsorsFailed::Unreadable(e.to_string()))?;
    Ok(entries
        .into_iter()
        .filter_map(|e| {
            let name = clean(&e.name);
            let github = e.github.filter(|login| is_login(login));
            match (name.is_empty(), github) {
                (true, None) => None,
                (true, Some(login)) => Some(Sponsor {
                    name: login.clone(),
                    github: Some(login),
                }),
                (false, github) => Some(Sponsor { name, github }),
            }
        })
        .collect())
}

/// `text` without control characters (escape sequences among them) or runs of space.
fn clean(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A GitHub login: letters, digits and single inner hyphens, at most 39.
fn is_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
}

/// Every sponsor's face, fetched side by side. A face that doesn't come
/// (no GitHub login, offline, not a picture) is their initial instead.
fn sources_of(sponsors: &[Sponsor]) -> Vec<Source> {
    let fetches: Vec<_> = sponsors
        .iter()
        .map(|s| {
            let login = s.github.clone();
            thread::spawn(move || login.and_then(|login| avatar(&login)))
        })
        .collect();
    fetches
        .into_iter()
        .zip(sponsors)
        .map(|(fetch, s)| match fetch.join().ok().flatten() {
            Some(picture) => Source::Picture(picture),
            None => initial(&s.name),
        })
        .collect()
}

fn avatar(login: &str) -> Option<RgbaImage> {
    let bytes = ureq::get(&format!("https://github.com/{login}.png?size=256"))
        .config()
        .timeout_global(Some(TIMEOUT))
        .build()
        .header("user-agent", USER_AGENT)
        .call()
        .ok()?
        .into_body()
        .with_config()
        .limit(MAX_AVATAR_BYTES)
        .read_to_vec()
        .ok()?;
    Some(image::load_from_memory(&bytes).ok()?.to_rgba8())
}

fn initial(name: &str) -> Source {
    let letter = name
        .chars()
        .next()
        .and_then(|c| c.to_uppercase().next())
        .filter(|c| c.width() == Some(1))
        .unwrap_or('?');
    let hash = name
        .chars()
        .fold(7u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
    Source::Initial {
        letter,
        color: HUES[hash as usize % HUES.len()],
    }
}

fn face(source: &Source, size: Size, kitty: Option<&mut Kitty>, font: (u32, u32)) -> Face {
    let (cols, rows) = (u32::from(size.cols), u32::from(size.rows));
    match source {
        Source::Picture(picture) => Face {
            size,
            pixels: round(picture, cols, rows * 2),
            image: kitty.and_then(|kitty| {
                kitty.upload(
                    &round(picture, cols * font.0, rows * font.1),
                    size.cols,
                    size.rows,
                    0,
                )
            }),
            letter: None,
        },
        Source::Initial { letter, color } => {
            let disc = RgbaImage::from_pixel(1, 1, Rgba([color[0], color[1], color[2], 255]));
            Face {
                size,
                pixels: round(&disc, cols, rows * 2),
                image: None,
                letter: Some((*letter, *color)),
            }
        }
    }
}

/// `picture` cropped square, scaled and cut to a circle centred in a
/// `width` × `height` canvas, with a softened edge.
fn round(picture: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let (w, h) = picture.dimensions();
    let side = w.min(h).max(1);
    let square =
        image::imageops::crop_imm(picture, (w - side) / 2, (h - side) / 2, side, side).to_image();
    let diameter = width.min(height).max(1);
    let scaled = image::imageops::resize(&square, diameter, diameter, FilterType::Lanczos3);
    let (left, top) = ((width - diameter) / 2, (height - diameter) / 2);
    let radius = diameter as f32 / 2.0;
    let mut canvas = RgbaImage::new(width, height);
    for (x, y, pixel) in scaled.enumerate_pixels() {
        let (dx, dy) = (x as f32 + 0.5 - radius, y as f32 + 0.5 - radius);
        let inside = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
        let mut pixel = *pixel;
        pixel[3] = (f32::from(pixel[3]) * inside) as u8;
        canvas.put_pixel(left + x, top + y, pixel);
    }
    canvas
}

/// The wall's content, top to bottom, in rows from its top.
struct Layout {
    height: u16,
    spotlight: Option<u16>,
    /// Where the grid starts, and cards per row.
    grid: Option<(u16, u16)>,
}

fn layout(width: u16, spotlight: bool, rest: usize) -> Layout {
    // Title, thanks, then a blank row.
    let mut y = 3;
    let spot = spotlight.then(|| {
        let at = y + 1;
        y = at + SPOT_HEIGHT + 2;
        at
    });
    let grid = (rest > 0).then(|| {
        let per_row = (width.saturating_sub(4) / CARD).clamp(1, rest as u16);
        let rows = (rest as u16).div_ceil(per_row);
        let at = y + 1;
        y = at + rows * (CARD_HEIGHT + ROW_GAP);
        (at, per_row)
    });
    Layout {
        height: y,
        spotlight: spot,
        grid,
    }
}

/// Draws the wall and returns the Kitty images to place.
fn draw(f: &mut Frame, wall: &mut Wall) -> Vec<(kitty::Image, Rect)> {
    let area = f.area();
    let view = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let plan = layout(view.width, wall.spotlight.is_some(), wall.rest.len());
    wall.scroll = wall.scroll.min(plan.height.saturating_sub(view.height));
    // Short walls sit in the middle of the screen; tall ones scroll.
    let top = view.height.saturating_sub(plan.height) / 2;
    let offset = i32::from(top) - i32::from(wall.scroll);
    let mut canvas = Canvas {
        buf: f.buffer_mut(),
        view,
        offset,
        wanted: Vec::new(),
    };

    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().add_modifier(Modifier::DIM);
    canvas.text(
        0,
        Line::from(vec![
            Span::styled("leaf", bold.fg(Color::Green)),
            Span::styled(" · ", dim),
            Span::styled("sponsors", bold),
        ]),
    );
    let thanks = if wall.total == 0 {
        "No sponsors yet. The first spot is open."
    } else {
        "leaf exists because of these wonderful people. Thank you, truly."
    };
    canvas.text(1, Line::styled(thanks, dim));

    if let (Some(y), Some(card)) = (plan.spotlight, &wall.spotlight) {
        let width = SPOT_WIDTH.min(view.width);
        let x = view.x + (view.width - width) / 2;
        canvas.frame(x, y, width);
        let face_x = view.x + view.width.saturating_sub(SPOT_FACE.cols) / 2;
        canvas.face(&card.face, face_x, y + 2);
        let below = y + 2 + SPOT_FACE.rows + 1;
        // Inside the border, so the box's sides stay whole.
        let inner = width.saturating_sub(2);
        canvas.span(x + 1, below, &centre(&card.sponsor.name, inner), bold);
        if let Some(login) = &card.sponsor.github {
            let login = centre(&format!("@{login}"), inner);
            canvas.span(x + 1, below + 1, &login, dim);
        }
    }

    if let Some((y, per_row)) = plan.grid {
        for (row, cards) in wall.rest.chunks(per_row as usize).enumerate() {
            let row_y = y + row as u16 * (CARD_HEIGHT + ROW_GAP);
            let row_width = cards.len() as u16 * CARD;
            let left = view.x + view.width.saturating_sub(row_width) / 2;
            for (n, card) in cards.iter().enumerate() {
                let x = left + n as u16 * CARD;
                canvas.face(&card.face, x + (CARD - GRID_FACE.cols) / 2, row_y);
                let name_y = row_y + GRID_FACE.rows + 1;
                canvas.span(x, name_y, &centre(&card.sponsor.name, CARD), bold);
                if let Some(login) = &card.sponsor.github {
                    canvas.span(x, name_y + 1, &centre(&format!("@{login}"), CARD), dim);
                }
            }
        }
    }

    let wanted = canvas.wanted;
    let more = plan.height > view.height;
    let footer = Line::from(vec![
        Span::styled("♥ ", Style::new().fg(Color::Red)),
        Span::styled("o", bold),
        Span::styled(" sponsor leaf", dim),
        Span::styled(if more { "   j/k " } else { "" }, bold),
        Span::styled(if more { "scroll" } else { "" }, dim),
        Span::styled("   q ", bold),
        Span::styled("quit", dim),
    ]);
    f.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        Rect {
            y: area.y + area.height.saturating_sub(1),
            height: area.height.min(1),
            ..area
        },
    );
    wanted
}

/// The screen, with the wall's rows shifted by the scroll.
struct Canvas<'a> {
    buf: &'a mut Buffer,
    view: Rect,
    offset: i32,
    wanted: Vec<(kitty::Image, Rect)>,
}

impl Canvas<'_> {
    /// The screen row of wall row `y`, if it's in view.
    fn row(&self, y: u16) -> Option<u16> {
        let screen = i32::from(y) + self.offset;
        (0..i32::from(self.view.height))
            .contains(&screen)
            .then(|| self.view.y + screen as u16)
    }

    fn text(&mut self, y: u16, line: Line) {
        if let Some(row) = self.row(y) {
            let area = Rect {
                y: row,
                height: 1,
                ..self.view
            };
            Paragraph::new(line)
                .alignment(Alignment::Center)
                .render(area, self.buf);
        }
    }

    fn span(&mut self, x: u16, y: u16, text: &str, style: Style) {
        if let Some(row) = self.row(y) {
            let room = self.view.right().saturating_sub(x);
            self.buf.set_stringn(x, row, text, room as usize, style);
        }
    }

    fn frame(&mut self, x: u16, y: u16, width: u16) {
        let (Some(top), Some(bottom)) = (self.row(y), self.row(y + SPOT_HEIGHT - 1)) else {
            // Partly scrolled out: the box is left off rather than cut open.
            return;
        };
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().add_modifier(Modifier::DIM))
            .title_top(
                Line::styled(" ✦ newest sponsor ✦ ", Style::new().fg(Color::Yellow))
                    .alignment(Alignment::Center),
            )
            .render(
                Rect {
                    x,
                    y: top,
                    width,
                    height: bottom - top + 1,
                },
                self.buf,
            );
    }

    fn face(&mut self, face: &Face, x: u16, y: u16) {
        if x + face.size.cols > self.view.right() {
            return;
        }
        if let Some(image) = face.image {
            // A picture only goes up whole; half in view would spill past the edge.
            if let (Some(top), Some(_)) = (self.row(y), self.row(y + face.size.rows - 1)) {
                self.wanted.push((
                    image,
                    Rect {
                        x,
                        y: top,
                        width: face.size.cols,
                        height: face.size.rows,
                    },
                ));
            }
            return;
        }
        let px = |cx: u16, py: u32| {
            let p = face.pixels.get_pixel(u32::from(cx), py);
            (p[3] >= 128).then_some(Color::Rgb(p[0], p[1], p[2]))
        };
        for cy in 0..face.size.rows {
            let Some(row) = self.row(y + cy) else {
                continue;
            };
            for cx in 0..face.size.cols {
                let (top, bottom) = (px(cx, u32::from(cy) * 2), px(cx, u32::from(cy) * 2 + 1));
                let cell = &mut self.buf[(x + cx, row)];
                match (top, bottom) {
                    (None, None) => {}
                    (Some(t), None) => {
                        cell.set_symbol("▀").set_fg(t);
                    }
                    (None, Some(b)) => {
                        cell.set_symbol("▄").set_fg(b);
                    }
                    (Some(t), Some(b)) => {
                        cell.set_symbol("▀").set_fg(t).set_bg(b);
                    }
                }
            }
        }
        if let Some((letter, [r, g, b])) = face.letter
            && let Some(row) = self.row(y + face.size.rows / 2)
        {
            let cell = &mut self.buf[(x + (face.size.cols - 1) / 2, row)];
            cell.set_char(letter)
                .set_fg(Color::Rgb(255, 255, 255))
                .set_bg(Color::Rgb(r, g, b))
                .modifier
                .insert(Modifier::BOLD);
        }
    }
}

fn print_plain(out: &mut impl Write, sponsors: &[Sponsor]) -> io::Result<()> {
    writeln!(out, "leaf · sponsors")?;
    writeln!(out)?;
    let newest = sponsors.iter().find(|s| s.is_newest());
    if let Some(s) = newest {
        writeln!(out, "Newest: {}", plain_name(s))?;
        writeln!(out)?;
    }
    for s in sponsors.iter().filter(|s| Some(*s) != newest) {
        writeln!(out, "{}", plain_name(s))?;
    }
    writeln!(out)?;
    writeln!(out, "Join them: leaf sponsor  {SPONSOR_URL}")
}

fn plain_name(s: &Sponsor) -> String {
    match &s.github {
        Some(login) => format!("{}  @{login}", s.name),
        None => s.name.clone(),
    }
}

/// `text` centred in `width` cells, cut short with `…` if it doesn't fit.
fn centre(text: &str, width: u16) -> String {
    let width = usize::from(width);
    let room = width.saturating_sub(2);
    let text = if text.width() > room {
        let mut cut = String::new();
        let mut used = 0;
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if used + w + 1 > room {
                break;
            }
            cut.push(c);
            used += w;
        }
        cut.push('…');
        cut
    } else {
        text.to_owned()
    };
    let spare = width.saturating_sub(text.width());
    let left = spare / 2;
    format!("{}{text}{}", " ".repeat(left), " ".repeat(spare - left))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sponsor(name: &str, github: Option<&str>) -> Sponsor {
        Sponsor {
            name: name.into(),
            github: github.map(Into::into),
        }
    }

    #[test]
    fn reads_the_site_list_and_cleans_what_strangers_typed() {
        let body = r#"[
            {"name":"Nick  Worrall","github":"xylophonehero"},
            {"name":"evil\u001b[2Jname"},
            {"name":"","github":"jensenojs"},
            {"name":"   "},
            {"name":"Bad Login","github":"../../x"}
        ]"#;
        let sponsors = parse(body).unwrap_or_default();
        assert_eq!(
            sponsors,
            vec![
                sponsor("Nick Worrall", Some("xylophonehero")),
                sponsor("evil [2Jname", None),
                sponsor("jensenojs", Some("jensenojs")),
                sponsor("Bad Login", None),
            ]
        );
    }

    #[test]
    fn an_unexpected_shape_is_reported() {
        assert!(matches!(
            parse(r#"{"error":"down"}"#),
            Err(SponsorsFailed::Unreadable(_))
        ));
    }

    #[test]
    fn logins_follow_githubs_rules() {
        assert!(is_login("johnny4dev"));
        assert!(is_login("a-b"));
        assert!(!is_login("-ab"));
        assert!(!is_login("a--b"));
        assert!(!is_login("a/b"));
        assert!(!is_login(&"a".repeat(40)));
    }

    #[test]
    fn the_newest_sponsor_is_matched_by_login() {
        assert!(sponsor("Ethan Troy", Some("EthanOliverTroy")).is_newest());
        assert!(!sponsor("Ethan Troy", None).is_newest());
        assert!(!sponsor("Nick", Some("xylophonehero")).is_newest());
    }

    #[test]
    fn names_are_centred_and_long_ones_cut() {
        assert_eq!(centre("Ethan", 9), "  Ethan  ");
        let cut = centre("A very long sponsor name", CARD);
        assert_eq!(cut.width(), usize::from(CARD));
        assert!(cut.trim().ends_with('…'));
    }

    #[test]
    fn faces_are_round_with_clear_corners() {
        let square = RgbaImage::from_pixel(40, 30, Rgba([200, 10, 10, 255]));
        let face = round(&square, 12, 12);
        assert_eq!(face.dimensions(), (12, 12));
        assert_eq!(face.get_pixel(0, 0)[3], 0);
        assert_eq!(face.get_pixel(6, 6)[3], 255);
    }

    #[test]
    fn the_grid_wraps_to_the_width_and_sits_below_the_spotlight() {
        let plan = layout(80, true, 7);
        let spot = plan.spotlight.unwrap_or_default();
        let (grid, per_row) = plan.grid.unwrap_or_default();
        assert_eq!(per_row, 4);
        assert!(grid >= spot + SPOT_HEIGHT);
        assert_eq!(plan.height, grid + 2 * (CARD_HEIGHT + ROW_GAP));
        assert_eq!(layout(200, false, 3).grid, Some((4, 3)));
    }

    fn screen(sponsors: Vec<Sponsor>, width: u16, height: u16) -> String {
        let sources: Vec<Source> = sponsors.iter().map(|s| initial(&s.name)).collect();
        let total = sponsors.len();
        let mut spotlight = None;
        let mut rest = Vec::new();
        for (sponsor, source) in sponsors.into_iter().zip(sources) {
            let newest = sponsor.is_newest();
            let size = if newest { SPOT_FACE } else { GRID_FACE };
            let card = Card {
                face: face(&source, size, None, (8, 16)),
                sponsor,
            };
            if newest {
                spotlight = Some(card);
            } else {
                rest.push(card);
            }
        }
        let mut wall = Wall {
            total,
            spotlight,
            rest,
            scroll: 0,
        };
        let Ok(mut terminal) = Terminal::new(TestBackend::new(width, height));
        let _ = terminal.draw(|f| {
            draw(f, &mut wall);
        });
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_newest_sponsor_is_in_the_spotlight_and_the_rest_in_the_grid() {
        let text = screen(
            vec![
                sponsor("jensenojs", Some("jensenojs")),
                sponsor("Ethan Troy", Some("ethanolivertroy")),
                sponsor("Johnny Heo", Some("johnny4dev")),
            ],
            80,
            40,
        );
        assert!(text.contains("leaf · sponsors"));
        assert!(text.contains("leaf exists because of these wonderful people. Thank you, truly."));
        assert!(text.contains("✦ newest sponsor ✦"));
        let spot = text.find("Ethan Troy").unwrap_or(usize::MAX);
        let grid = text.find("Johnny Heo").unwrap_or(0);
        assert!(spot < grid, "the spotlight comes before the grid");
        let name_line = text.lines().find(|l| l.contains("Ethan Troy"));
        assert_eq!(
            name_line.map(|l| l.matches('│').count()),
            Some(2),
            "the name sits inside the box's sides"
        );
        assert!(text.contains("q quit"));
    }

    #[test]
    fn a_small_screen_scrolls_instead_of_breaking() {
        let many = (0..12)
            .map(|n| sponsor(&format!("Person {n}"), None))
            .collect();
        let text = screen(many, 30, 12);
        assert!(text.contains("j/k scroll"));
    }

    #[test]
    fn a_tiny_or_empty_screen_draws_without_panicking() {
        for (width, height) in [(0, 0), (1, 1), (10, 5), (19, 30)] {
            screen(
                vec![
                    sponsor("Ethan Troy", Some("ethanolivertroy")),
                    sponsor("Ada", None),
                ],
                width,
                height,
            );
        }
    }

    #[test]
    fn plain_output_lists_the_newest_first() {
        let sponsors = vec![
            sponsor("Nick Worrall", Some("xylophonehero")),
            sponsor("Ethan Troy", Some("ethanolivertroy")),
            sponsor("Ada", None),
        ];
        let mut out = Vec::new();
        assert!(print_plain(&mut out, &sponsors).is_ok());
        let text = String::from_utf8(out).unwrap_or_default();
        assert!(text.contains("Newest: Ethan Troy  @ethanolivertroy"));
        assert!(text.contains("\nNick Worrall  @xylophonehero\nAda\n"));
        assert!(!text.contains('\x1b'));
    }
}
