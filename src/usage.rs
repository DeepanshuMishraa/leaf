//! How much leaf is used, day by day, kept only in the local store: time
//! with a document, a diff or a pull request open, counted while someone is
//! at it. A gap between keys longer than `IDLE` counts as `IDLE`, so a
//! reader left open overnight isn't a night of reading.

use std::time::{Duration, Instant};

use crate::progress::Store;

/// The longest gap between keys that still counts in full.
const IDLE: Duration = Duration::from_secs(5 * 60);
/// How often counted time is written to the store.
const FLUSH: Duration = Duration::from_secs(60);
const DAY: i64 = 24 * 60 * 60;

/// A local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    /// 0 is Monday, 6 Sunday.
    pub weekday: u8,
}

impl Date {
    /// The day `n` days before today, in local time.
    pub fn days_ago(n: u32) -> Date {
        // From noon, so a daylight-saving hour never lands on the wrong day.
        Date::at(noon_today() - i64::from(n) * DAY)
    }

    pub fn today() -> Date {
        Date::days_ago(0)
    }

    fn at(time: i64) -> Date {
        let tm = local(time);
        Date {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u8,
            day: tm.tm_mday as u8,
            weekday: ((tm.tm_wday + 6) % 7) as u8,
        }
    }

    /// `YYYY-MM-DD`, the store's key, which sorts by date.
    pub fn key(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    pub fn month_name(self) -> &'static str {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        MONTHS[(self.month.clamp(1, 12) - 1) as usize]
    }

    pub fn weekday_name(self) -> &'static str {
        const DAYS: [&str; 7] = [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ];
        DAYS[self.weekday.min(6) as usize]
    }
}

/// The local hour now, 0 to 23.
pub fn hour() -> u8 {
    local(now()).tm_hour as u8
}

fn now() -> i64 {
    // SAFETY: time(NULL) only reads the clock.
    unsafe { libc::time(std::ptr::null_mut()) as i64 }
}

fn local(time: i64) -> libc::tm {
    let time = time as libc::time_t;
    // SAFETY: localtime_r fills the tm it's given and keeps no pointer to it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&time, &mut tm);
        tm
    }
}

fn noon_today() -> i64 {
    let mut tm = local(now());
    tm.tm_hour = 12;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    // SAFETY: mktime reads and normalises the tm it's given.
    unsafe { libc::mktime(&mut tm) as i64 }
}

/// Counts time spent in a view, writing it to the store now and then and
/// when it's dropped. A store that fails loses only the count.
pub struct Clock {
    store: Option<Store>,
    last: Instant,
    counted: Duration,
    flushed: Instant,
}

impl Clock {
    pub fn start() -> Self {
        let now = Instant::now();
        Clock {
            store: store(),
            last: now,
            counted: Duration::ZERO,
            flushed: now,
        }
    }

    /// Someone did something: the time since the last touch counts, up to `IDLE`.
    pub fn touch(&mut self) {
        let now = Instant::now();
        self.counted += now.duration_since(self.last).min(IDLE);
        self.last = now;
        if now.duration_since(self.flushed) >= FLUSH {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.flushed = Instant::now();
        let seconds = self.counted.as_secs();
        if seconds == 0 {
            return;
        }
        if let Some(store) = &self.store
            && store.add_usage(&Date::today().key(), seconds).is_ok()
        {
            self.counted -= Duration::from_secs(seconds);
        }
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.touch();
        self.flush();
    }
}

#[cfg(not(test))]
fn store() -> Option<Store> {
    Store::open(&crate::progress::default_path()).ok()
}

#[cfg(test)]
fn store() -> Option<Store> {
    None
}

/// How long `seconds` is, briefly: `under a minute`, `12m`, `1h 12m`.
pub fn brief(seconds: u64) -> String {
    let minutes = seconds / 60;
    match (minutes / 60, minutes % 60) {
        (0, 0) => "under a minute".to_owned(),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_step_back_one_at_a_time_and_key_in_order() {
        let today = Date::today();
        let yesterday = Date::days_ago(1);
        assert!(yesterday.key() < today.key());
        assert_eq!((today.weekday + 6) % 7, yesterday.weekday);
        let week = Date::days_ago(7);
        assert_eq!(week.weekday, today.weekday);
        assert_eq!(today.key().len(), 10);
    }

    #[test]
    fn durations_read_briefly() {
        assert_eq!(brief(20), "under a minute");
        assert_eq!(brief(12 * 60), "12m");
        assert_eq!(brief(3600), "1h");
        assert_eq!(brief(3600 + 12 * 60 + 5), "1h 12m");
    }

    #[test]
    fn long_gaps_count_only_up_to_idle() {
        let mut clock = Clock::start();
        // A machine up for less than this can't go back that far; nothing to check then.
        let Some(earlier) = Instant::now().checked_sub(IDLE * 3) else {
            return;
        };
        clock.last = earlier;
        clock.touch();
        assert_eq!(clock.counted, IDLE);
    }
}
