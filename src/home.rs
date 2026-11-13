//! The home screen `leaf` opens on when nothing is named: a greeting, how
//! long leaf was used today, a dithered heat map of the days it was used,
//! then a search over recent documents and the folder it started in. The
//! heat map and progress meters are dithered whatever the UI setting; the
//! margins and bar follow it.

use std::collections::HashMap;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::art::{self, Ink};
use crate::picker::{self, Entry};
use crate::render::DIM;
use crate::usage::{self, Date};

/// Widest the home column gets.
const WIDTH: u16 = 76;
/// Columns a heat map week takes: a two-column day and a gap.
const WEEK: u16 = 3;
/// Most weeks the heat map shows: a year.
const MAX_WEEKS: u16 = 53;
/// The heat map's height with its month, gap and legend rows, and the space after it.
const HEAT_ROWS: u16 = art::HEAT_GRID_ROWS + 4;
/// Screens shorter than this leave the heat map out, keeping the list.
const HEAT_MIN_HEIGHT: u16 = 34;
/// Columns the mini progress meter beside a recent document takes.
const METER: u16 = 8;

pub struct View<'a> {
    pub name: &'a str,
    /// Seconds used, by day key.
    pub usage: &'a HashMap<String, u64>,
    pub typed: &'a str,
    /// Where the picker started, so recent paths read short.
    pub here: &'a Path,
    pub entries: &'a [(String, Vec<Entry>)],
    pub selected: usize,
    /// The theme's tints, for the dither that always shows.
    pub ink: Ink,
    /// Whether the dither UI is on, for the margins and bar.
    pub dither: bool,
}

pub fn draw(f: &mut Frame, view: &View) {
    let area = f.area();
    if area.width < 20 || area.height < 8 {
        return;
    }
    let width = area.width.saturating_sub(4).min(WIDTH);
    let x = area.x + (area.width - width) / 2;
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(DIM);
    let footer_y = area.bottom() - 1;

    // Home stays quiet: no margins, just the bar the keys sit on.
    if view.dither {
        art::surface(
            f.buffer_mut(),
            Rect::new(area.x, footer_y, area.width, 1),
            view.ink,
        );
        art::dissolve(
            f.buffer_mut(),
            Rect::new(area.x, footer_y - 1, area.width, 1),
            view.ink,
            art::Bar::Below,
        );
    }

    let roomy = area.height >= HEAT_MIN_HEIGHT;
    let mut y = area.y + if roomy { 2 } else { 1 };
    let today = Date::today();

    // The masthead: leaf, and the date.
    let date = format!(
        "{}, {} {}",
        today.weekday_name(),
        today.day,
        today.month_name()
    );
    f.render_widget(
        Paragraph::new(spread(
            vec![Span::styled("leaf", bold)],
            vec![Span::styled(date, dim)],
            width,
        )),
        Rect::new(x, y, width, 1),
    );
    y += 2;

    // The greeting, and today in a line.
    let greeting = match view.name {
        "" => greeting(usage::hour()).to_owned(),
        name => format!("{}, {name}", greeting(usage::hour())),
    };
    f.render_widget(
        Paragraph::new(Span::styled(greeting, bold)),
        Rect::new(x, y, width, 1),
    );
    y += 1;
    let spent = view.usage.get(&today.key()).copied().unwrap_or(0);
    let streak = streak(view.usage);
    let mut summary = if spent == 0 {
        "Nothing read yet today".to_owned()
    } else {
        format!("{} in leaf today", usage::brief(spent))
    };
    if streak > 1 {
        summary.push_str(&format!(" · {streak}-day streak"));
    }
    f.render_widget(
        Paragraph::new(Span::styled(summary, dim)),
        Rect::new(x, y, width, 1),
    );
    y += 2;

    if roomy {
        heat_map(f, view, Rect::new(x, y, width, HEAT_ROWS - 1));
        y += HEAT_ROWS;
    }

    // The search field.
    let (folder, filter) = field_parts(view.typed, view.here);
    let field = Rect::new(x, y, width, 1);
    art::surface(f.buffer_mut(), field, view.ink);
    let mut spans = vec![Span::styled(" ⌕  ", dim)];
    if filter.is_empty() && folder.is_empty() {
        spans.push(Span::styled("▏", bold));
        spans.push(Span::styled(
            "Search recent documents, or type a path or link",
            dim,
        ));
    } else {
        spans.push(Span::styled(folder, dim));
        spans.push(Span::styled(filter, bold));
        spans.push(Span::styled("▏", bold));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), field);
    y += 2;

    // The list, scrolled to keep the picked row in view.
    let bottom = footer_y.saturating_sub(1);
    list(f, view, Rect::new(x, y, width, bottom.saturating_sub(y)));

    let keys = "↑↓ move · enter open · tab into folder · esc quit";
    f.render_widget(
        Paragraph::new(Span::styled(keys, dim)).centered(),
        Rect::new(area.x, footer_y, area.width, 1),
    );
}

/// Good morning, afternoon, evening or night, by the local hour.
fn greeting(hour: u8) -> &'static str {
    match hour {
        5..=11 => "Good morning",
        12..=16 => "Good afternoon",
        17..=21 => "Good evening",
        _ => "Good night",
    }
}

/// Days in a row leaf was used, up to today, or up to yesterday while today
/// has none yet.
fn streak(usage: &HashMap<String, u64>) -> u32 {
    let used = |n: u32| usage.get(&Date::days_ago(n).key()).is_some_and(|&s| s > 0);
    let first = if used(0) { 0 } else { 1 };
    (first..).take_while(|&n| used(n)).count() as u32
}

/// How full a day's heat map cell is.
fn level(seconds: u64) -> u8 {
    match seconds {
        0 => 0,
        1..900 => 1,
        900..2700 => 2,
        2700..7200 => 3,
        _ => 4,
    }
}

/// Weeks as columns, Monday to Sunday down each, this week last; month
/// names above, a legend and the total below.
fn heat_map(f: &mut Frame, view: &View, area: Rect) {
    let dim = Style::default().fg(DIM);
    let label = 4;
    let weeks = ((area.width.saturating_sub(label) + 1) / WEEK).clamp(1, MAX_WEEKS);
    let grid_x = area.x + label;
    let top = area.y + 1;
    let today = Date::today();
    // Days back from today to the Monday of the oldest week shown.
    let span = u32::from(weeks - 1) * 7 + u32::from(today.weekday);

    let mut days = vec![[None; 7]; weeks as usize];
    let mut months_end = 0;
    let mut total = 0;
    for (week, column) in days.iter_mut().enumerate() {
        let cell_x = grid_x + week as u16 * WEEK;
        for (row, slot) in column.iter_mut().enumerate() {
            // Days after today in this week aren't drawn.
            let Some(back) = span.checked_sub(week as u32 * 7 + row as u32) else {
                continue;
            };
            let date = Date::days_ago(back);
            let seconds = view.usage.get(&date.key()).copied().unwrap_or(0);
            total += seconds;
            *slot = Some(level(seconds));
            // A month's name over the week its first Monday falls in.
            if row == 0 && date.day <= 7 && cell_x >= months_end {
                let name = date.month_name();
                f.buffer_mut().set_string(cell_x, area.y, name, dim);
                months_end = cell_x + name.len() as u16 + 1;
            }
        }
    }
    art::heat_grid(f.buffer_mut(), (grid_x, top), &days, view.ink);
    // Each day is three pixels down, two to a row: Monday, Wednesday and
    // Friday start on rows 0, 3 and 6.
    for (row, name) in [(0, "Mon"), (3, "Wed"), (6, "Fri")] {
        f.buffer_mut().set_string(area.x, top + row, name, dim);
    }

    // The legend: the total on the left, the scale on the right.
    let legend_y = top + art::HEAT_GRID_ROWS + 1;
    let months = (u32::from(weeks) * 7).div_ceil(30);
    let said = if total == 0 {
        "Open a document and the days fill in".to_owned()
    } else {
        format!(
            "{} in the last {months} month{}",
            usage::brief(total),
            if months == 1 { "" } else { "s" }
        )
    };
    f.buffer_mut().set_string(area.x, legend_y, said, dim);
    let scale_w = 4 + 1 + HEAT_LEVELS_SHOWN * WEEK + 4;
    let scale_x = (grid_x + weeks * WEEK).saturating_sub(scale_w + 1);
    f.buffer_mut().set_string(scale_x, legend_y, "less", dim);
    for n in 0..HEAT_LEVELS_SHOWN {
        art::heat(
            f.buffer_mut(),
            Rect::new(scale_x + 5 + n * WEEK, legend_y, 2, 1),
            n as u8,
            view.ink,
        );
    }
    f.buffer_mut().set_string(
        scale_x + 5 + HEAT_LEVELS_SHOWN * WEEK,
        legend_y,
        "more",
        dim,
    );
}

/// Swatches in the legend: every level, empty to full.
const HEAT_LEVELS_SHOWN: u16 = art::HEAT_LEVELS as u16 + 1;

/// The field as shown: the folder it's in, shortened, and what's typed after it.
fn field_parts(typed: &str, here: &Path) -> (String, String) {
    let start = format!("{}/", here.display().to_string().trim_end_matches('/'));
    if let Some(filter) = typed.strip_prefix(&start)
        && !filter.contains('/')
    {
        return (String::new(), filter.to_owned());
    }
    match typed.rfind('/') {
        Some(at) => (picker::tilde(&typed[..=at]), typed[at + 1..].to_owned()),
        None => (String::new(), typed.to_owned()),
    }
}

fn list(f: &mut Frame, view: &View, area: Rect) {
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    if area.height == 0 {
        return;
    }
    // Every row: a section title, or an entry and its index among entries.
    let mut rows: Vec<Option<(usize, &Entry)>> = Vec::new();
    let mut titles: HashMap<usize, &str> = HashMap::new();
    let mut index = 0;
    for (title, entries) in view.entries.iter().filter(|(_, e)| !e.is_empty()) {
        if !rows.is_empty() {
            rows.push(None);
        }
        if !title.is_empty() {
            titles.insert(rows.len(), title);
            rows.push(None);
        }
        for entry in entries {
            rows.push(Some((index, entry)));
            index += 1;
        }
    }
    if index == 0 {
        f.render_widget(
            Paragraph::new(Span::styled(
                "Nothing here answers that. Type a path, or an https:// link.",
                dim,
            )),
            Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), 1),
        );
        return;
    }
    let picked_row = rows
        .iter()
        .position(|row| row.is_some_and(|(i, _)| i == view.selected))
        .unwrap_or(0);
    let room = area.height as usize;
    let first = picked_row.saturating_sub(room.saturating_sub(1));
    for (offset, row) in rows.iter().enumerate().skip(first).take(room) {
        let y = area.y + (offset - first) as u16;
        let line = Rect::new(area.x, y, area.width, 1);
        if let Some(title) = titles.get(&offset) {
            f.buffer_mut().set_string(
                area.x + 1,
                y,
                title.to_uppercase(),
                dim.add_modifier(Modifier::BOLD),
            );
            continue;
        }
        let Some((i, entry)) = row else {
            continue;
        };
        let picked = *i == view.selected;
        if picked {
            art::surface(f.buffer_mut(), line, view.ink);
        }
        let (mark, name, place, percent) = describe(entry, view.here);
        let name_style = if picked { bold } else { Style::default() };
        let right = if percent.is_some() { METER + 6 } else { 0 };
        let room = (area.width as usize).saturating_sub(4 + right as usize);
        let name = clip(&name, room);
        let place = clip(&place, room.saturating_sub(name.width() + 2));
        let spans = vec![
            Span::styled(if picked { " › " } else { "   " }, bold),
            Span::styled(mark, dim),
            Span::styled(name, name_style),
            Span::styled(
                if place.is_empty() {
                    String::new()
                } else {
                    format!("  {place}")
                },
                dim,
            ),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), line);
        if let Some(percent) = percent {
            let meter_x = line.right().saturating_sub(right);
            art::progress(
                f.buffer_mut(),
                Rect::new(meter_x, y, METER, 1),
                percent as f32 / 100.0,
                view.ink,
            );
            f.buffer_mut()
                .set_string(meter_x + METER + 1, y, format!("{percent:>3}%"), dim);
        }
    }
}

/// An entry as the list shows it: a mark, its name, where it is, and how
/// far it was read.
fn describe(entry: &Entry, here: &Path) -> (&'static str, String, String, Option<usize>) {
    let split = |path: &Path| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let parent = path
            .parent()
            .map(|p| {
                p.strip_prefix(here)
                    .ok()
                    .filter(|r| !r.as_os_str().is_empty())
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|| picker::tilde(&p.display().to_string()))
            })
            .unwrap_or_default();
        (name, parent)
    };
    match entry {
        Entry::Folder(path) => {
            let (name, _) = split(path);
            ("▸ ", format!("{name}/"), String::new(), None)
        }
        Entry::Document(path) => {
            let (name, _) = split(path);
            ("  ", name, String::new(), None)
        }
        Entry::Recent { target, percent } if picker::is_link(target) => {
            ("↗ ", target.clone(), String::new(), Some(*percent))
        }
        Entry::Recent { target, percent } => {
            let (name, parent) = split(Path::new(target));
            ("  ", name, parent, Some(*percent))
        }
        Entry::Link(url) => ("↗ ", format!("Read {url}"), String::new(), None),
    }
}

/// `text` cut to `width` columns, with an ellipsis when cut.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + 2 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// `left` and `right` on one line, `width` wide.
fn spread<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, width: u16) -> Line<'a> {
    let used: usize = left.iter().chain(&right).map(|s| s.content.width()).sum();
    let gap = (width as usize).saturating_sub(used).max(1);
    Line::from([left, vec![Span::raw(" ".repeat(gap))], right].concat())
}

/// The first name of the account leaf runs as, from its full name in the
/// system's user records, else the login name; empty when neither says.
pub fn first_name() -> String {
    // SAFETY: getpwuid returns a pointer into a static record, read at once
    // and copied out before anything else could call it.
    let full = unsafe {
        let record = libc::getpwuid(libc::getuid());
        if record.is_null() || (*record).pw_gecos.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr((*record).pw_gecos)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    let first = |text: &str| {
        text.split(',')
            .next()
            .and_then(|name| name.split_whitespace().next())
            .map(str::to_owned)
    };
    full.as_deref()
        .and_then(first)
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .filter(|u| !u.is_empty())
                .map(|u| {
                    let mut chars = u.chars();
                    chars
                        .next()
                        .map(|c| c.to_uppercase().chain(chars).collect())
                        .unwrap_or_default()
                })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_greeting_follows_the_hour() {
        assert_eq!(greeting(7), "Good morning");
        assert_eq!(greeting(13), "Good afternoon");
        assert_eq!(greeting(19), "Good evening");
        assert_eq!(greeting(2), "Good night");
    }

    #[test]
    fn days_fill_in_by_how_long_they_were() {
        assert_eq!(level(0), 0);
        assert_eq!(level(60), 1);
        assert_eq!(level(30 * 60), 2);
        assert_eq!(level(90 * 60), 3);
        assert_eq!(level(5 * 3600), 4);
    }

    #[test]
    fn a_streak_counts_back_from_today_or_yesterday() {
        let day = |n| (Date::days_ago(n).key(), 600);
        let usage: HashMap<String, u64> = [day(0), day(1), day(2), day(4)].into();
        assert_eq!(streak(&usage), 3);
        let usage: HashMap<String, u64> = [day(1), day(2)].into();
        assert_eq!(streak(&usage), 2, "today hasn't broken it yet");
        assert_eq!(streak(&HashMap::new()), 0);
    }

    #[test]
    fn the_field_shows_the_filter_alone_where_it_started() {
        let here = Path::new("/home/a/docs");
        assert_eq!(
            field_parts("/home/a/docs/pap", here),
            (String::new(), "pap".to_owned())
        );
        assert_eq!(
            field_parts("/elsewhere/books/x", here),
            ("/elsewhere/books/".to_owned(), "x".to_owned())
        );
    }
}
