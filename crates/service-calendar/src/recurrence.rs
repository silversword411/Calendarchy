//! A small RFC 5545 RRULE subset — just enough to drive the edit dialog's "Does not
//! repeat" dropdown and its "Custom recurrence" picker (DESIGN_SPEC.md §10). Google's
//! recurring-event model (single instance vs. series vs. "this and following") is
//! explicitly out of scope for v1 (§20) — a `Recurrence` here describes the *whole*
//! series, stored as one `RRULE:` line on the event's own row, with no per-instance
//! expansion or exception handling (`EXDATE`/`RDATE`) attempted.
use chrono::{Datelike, Duration, NaiveDate, Weekday};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Frequency {
    fn as_str(self) -> &'static str {
        match self {
            Frequency::Daily => "DAILY",
            Frequency::Weekly => "WEEKLY",
            Frequency::Monthly => "MONTHLY",
            Frequency::Yearly => "YEARLY",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "DAILY" => Some(Frequency::Daily),
            "WEEKLY" => Some(Frequency::Weekly),
            "MONTHLY" => Some(Frequency::Monthly),
            "YEARLY" => Some(Frequency::Yearly),
            _ => None,
        }
    }
}

/// When a series stops. `Never` is the common case (and Google's own default: an
/// `RRULE` with neither `UNTIL` nor `COUNT` repeats forever).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurrenceEnd {
    Never,
    OnDate(NaiveDate),
    AfterCount(u32),
}

/// One event series' repeat rule. `by_day` only applies to `Weekly` (the "Repeat on"
/// day toggles in the custom-recurrence dialog); `monthly_ordinal` is only ever
/// produced by the "Monthly on the Nth `<weekday>`" smart preset (`monthly_on`) — the
/// custom dialog has no UI for picking a monthly by-weekday rule (matching the
/// reference screenshot, which shows no such control), so `Custom…` on `Monthly`/
/// `Yearly` always falls back to the plain "same day of month"/"same day of year"
/// rule implied by leaving both unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recurrence {
    pub freq: Frequency,
    pub interval: u32,
    pub by_day: Vec<Weekday>,
    pub monthly_ordinal: Option<(i32, Weekday)>,
    pub end: RecurrenceEnd,
}

fn weekday_code(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

fn parse_weekday_code(s: &str) -> Option<Weekday> {
    match s {
        "MO" => Some(Weekday::Mon),
        "TU" => Some(Weekday::Tue),
        "WE" => Some(Weekday::Wed),
        "TH" => Some(Weekday::Thu),
        "FR" => Some(Weekday::Fri),
        "SA" => Some(Weekday::Sat),
        "SU" => Some(Weekday::Sun),
        _ => None,
    }
}

/// Full weekday name, e.g. for "Weekly on Tuesday" — `chrono::Weekday`'s own `Display`
/// prints the 3-letter abbreviation, not the full name the dropdown/label text wants.
pub fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

/// A `BYDAY` token is an optional signed ordinal followed by a 2-letter weekday code
/// (`"TU"`, `"1TU"`, `"-1FR"`). Returns `(ordinal, weekday)`, with `ordinal == 0`
/// meaning "no ordinal prefix" (the plain weekly case).
fn parse_byday_token(token: &str) -> Option<(i32, Weekday)> {
    let token = token.trim();
    if token.len() < 2 {
        return None;
    }
    let split_at = token.len() - 2;
    let (ordinal_str, code) = token.split_at(split_at);
    let day = parse_weekday_code(code)?;
    let ordinal = if ordinal_str.is_empty() { 0 } else { ordinal_str.parse().ok()? };
    Some((ordinal, day))
}

fn ordinal_word(n: i32) -> &'static str {
    match n {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        5 => "fifth",
        _ => "last",
    }
}

/// This weekday's position within its month, as an `RRULE` `BYDAY` ordinal: `1` for
/// the 1st occurrence of that weekday in the month, `2` for the 2nd, and so on, or
/// `-1` if `date` is that weekday's *last* occurrence in the month (matching Google's
/// own "Monthly on the last `<weekday>`" wording for e.g. the 5th Tuesday of a month
/// that only has four full weeks after it).
fn monthly_ordinal_for(date: NaiveDate) -> (i32, Weekday) {
    let ordinal = (date.day0() / 7) as i32 + 1;
    let is_last = (date + Duration::days(7)).month() != date.month();
    (if is_last { -1 } else { ordinal }, date.weekday())
}

/// `by_day` covers exactly Monday through Friday, in any order — the set an "Every
/// weekday" rule's `BYDAY` normalizes to, so `describe` can collapse a custom weekly
/// rule that happens to match back to that preset's own label.
fn is_every_weekday(by_day: &[Weekday]) -> bool {
    let mut days: Vec<Weekday> = by_day.to_vec();
    days.sort_by_key(|d| d.num_days_from_monday());
    days.dedup();
    days.len() == 5 && days.iter().all(|d| d.num_days_from_monday() < 5)
}

impl Recurrence {
    /// Serializes to an RRULE value (everything after `"RRULE:"` — callers that need
    /// the full Google-style line prepend that themselves, matching how
    /// `storage::upsert_event` already treats `recurrence_rule` as a raw string).
    pub fn to_rrule_string(&self) -> String {
        let mut parts = vec![format!("FREQ={}", self.freq.as_str())];
        if self.interval > 1 {
            parts.push(format!("INTERVAL={}", self.interval));
        }
        if let Some((ordinal, day)) = self.monthly_ordinal {
            parts.push(format!("BYDAY={ordinal}{}", weekday_code(day)));
        } else if !self.by_day.is_empty() {
            let days: Vec<&str> = self.by_day.iter().map(|d| weekday_code(*d)).collect();
            parts.push(format!("BYDAY={}", days.join(",")));
        }
        match &self.end {
            RecurrenceEnd::Never => {}
            RecurrenceEnd::OnDate(date) => parts.push(format!("UNTIL={}", date.format("%Y%m%d"))),
            RecurrenceEnd::AfterCount(n) => parts.push(format!("COUNT={n}")),
        }
        parts.join(";")
    }

    /// Parses an RRULE value back out, accepting either a bare value or a full
    /// `"RRULE:..."` line. Unknown/malformed parts are skipped rather than failing the
    /// whole parse, so a stray Google extension this app doesn't understand degrades
    /// to "as much of the rule as was recognized" instead of "does not repeat".
    pub fn from_rrule_string(s: &str) -> Option<Recurrence> {
        let s = s.strip_prefix("RRULE:").unwrap_or(s);
        let mut freq = None;
        let mut interval = 1u32;
        let mut by_day_tokens: Vec<&str> = Vec::new();
        let mut end = RecurrenceEnd::Never;

        for part in s.split(';') {
            let Some((key, value)) = part.split_once('=') else { continue };
            match key {
                "FREQ" => freq = Frequency::parse(value),
                "INTERVAL" => interval = value.parse().unwrap_or(1).max(1),
                "BYDAY" => by_day_tokens = value.split(',').collect(),
                "COUNT" => {
                    if let Ok(n) = value.parse() {
                        end = RecurrenceEnd::AfterCount(n);
                    }
                }
                "UNTIL" => {
                    let date_part = &value[..value.len().min(8)];
                    if let Ok(date) = NaiveDate::parse_from_str(date_part, "%Y%m%d") {
                        end = RecurrenceEnd::OnDate(date);
                    }
                }
                _ => {}
            }
        }

        let freq = freq?;
        let mut by_day = Vec::new();
        let mut monthly_ordinal = None;
        for token in by_day_tokens {
            let Some((ordinal, day)) = parse_byday_token(token) else { continue };
            if ordinal != 0 {
                monthly_ordinal = Some((ordinal, day));
            } else {
                by_day.push(day);
            }
        }

        Some(Recurrence { freq, interval, by_day, monthly_ordinal, end })
    }

    /// A human-readable label for the repeat button/dropdown, e.g. "Weekly on
    /// Tuesday", "Every 2 weeks on Monday, Wednesday, until Dec 1, 2026", or "Every
    /// weekday (Monday to Friday)" — `start` (the event's own start date) fills in
    /// whatever the rule itself leaves implicit (the day-of-week for a plain weekly
    /// rule with no `by_day`, the month/day for `Yearly`).
    pub fn describe(&self, start: NaiveDate) -> String {
        let base = self.describe_base(start);
        match &self.end {
            RecurrenceEnd::Never => base,
            RecurrenceEnd::OnDate(date) => format!("{base}, until {}", date.format("%b %-d, %Y")),
            RecurrenceEnd::AfterCount(n) => format!("{base}, {n} time{}", if *n == 1 { "" } else { "s" }),
        }
    }

    fn describe_base(&self, start: NaiveDate) -> String {
        match self.freq {
            Frequency::Daily => {
                if self.interval <= 1 {
                    "Daily".to_string()
                } else {
                    format!("Every {} days", self.interval)
                }
            }
            Frequency::Weekly => {
                if self.interval <= 1 && is_every_weekday(&self.by_day) {
                    return "Every weekday (Monday to Friday)".to_string();
                }
                let days = if self.by_day.is_empty() {
                    weekday_name(start.weekday()).to_string()
                } else {
                    self.by_day.iter().map(|d| weekday_name(*d)).collect::<Vec<_>>().join(", ")
                };
                if self.interval <= 1 {
                    format!("Weekly on {days}")
                } else {
                    format!("Every {} weeks on {days}", self.interval)
                }
            }
            Frequency::Monthly => {
                if let Some((ordinal, day)) = self.monthly_ordinal {
                    let word = ordinal_word(ordinal);
                    if self.interval <= 1 {
                        format!("Monthly on the {word} {}", weekday_name(day))
                    } else {
                        format!("Every {} months on the {word} {}", self.interval, weekday_name(day))
                    }
                } else if self.interval <= 1 {
                    format!("Monthly on day {}", start.day())
                } else {
                    format!("Every {} months on day {}", self.interval, start.day())
                }
            }
            Frequency::Yearly => {
                if self.interval <= 1 {
                    format!("Annually on {}", start.format("%B %-d"))
                } else {
                    format!("Every {} years on {}", self.interval, start.format("%B %-d"))
                }
            }
        }
    }
}

pub fn daily() -> Recurrence {
    Recurrence {
        freq: Frequency::Daily,
        interval: 1,
        by_day: Vec::new(),
        monthly_ordinal: None,
        end: RecurrenceEnd::Never,
    }
}

pub fn weekly_on(start: NaiveDate) -> Recurrence {
    Recurrence {
        freq: Frequency::Weekly,
        interval: 1,
        by_day: vec![start.weekday()],
        monthly_ordinal: None,
        end: RecurrenceEnd::Never,
    }
}

pub fn monthly_on(start: NaiveDate) -> Recurrence {
    Recurrence {
        freq: Frequency::Monthly,
        interval: 1,
        by_day: Vec::new(),
        monthly_ordinal: Some(monthly_ordinal_for(start)),
        end: RecurrenceEnd::Never,
    }
}

pub fn yearly_on() -> Recurrence {
    Recurrence {
        freq: Frequency::Yearly,
        interval: 1,
        by_day: Vec::new(),
        monthly_ordinal: None,
        end: RecurrenceEnd::Never,
    }
}

pub fn every_weekday() -> Recurrence {
    Recurrence {
        freq: Frequency::Weekly,
        interval: 1,
        by_day: vec![Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri],
        monthly_ordinal: None,
        end: RecurrenceEnd::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn presets_describe_as_expected() {
        // 2026-09-01 is a Tuesday.
        let start = date(2026, 9, 1);
        assert_eq!(daily().describe(start), "Daily");
        assert_eq!(weekly_on(start).describe(start), "Weekly on Tuesday");
        assert_eq!(monthly_on(start).describe(start), "Monthly on the first Tuesday");
        assert_eq!(yearly_on().describe(start), "Annually on September 1");
        assert_eq!(every_weekday().describe(start), "Every weekday (Monday to Friday)");
    }

    #[test]
    fn monthly_ordinal_detects_last_occurrence_in_month() {
        // 2026-09-29 is the fifth (and last) Tuesday of September 2026.
        let start = date(2026, 9, 29);
        assert_eq!(monthly_on(start).describe(start), "Monthly on the last Tuesday");
    }

    #[test]
    fn rrule_round_trips_through_string_form() {
        let start = date(2026, 9, 1);
        for recurrence in [
            daily(),
            weekly_on(start),
            monthly_on(start),
            yearly_on(),
            every_weekday(),
            Recurrence {
                freq: Frequency::Weekly,
                interval: 2,
                by_day: vec![Weekday::Mon, Weekday::Wed],
                monthly_ordinal: None,
                end: RecurrenceEnd::AfterCount(13),
            },
            Recurrence {
                freq: Frequency::Daily,
                interval: 3,
                by_day: vec![],
                monthly_ordinal: None,
                end: RecurrenceEnd::OnDate(date(2026, 12, 1)),
            },
        ] {
            let s = recurrence.to_rrule_string();
            let parsed = Recurrence::from_rrule_string(&s).unwrap_or_else(|| panic!("failed to parse {s}"));
            assert_eq!(parsed, recurrence, "round trip through {s}");
        }
    }

    #[test]
    fn from_rrule_string_strips_rrule_prefix() {
        let parsed = Recurrence::from_rrule_string("RRULE:FREQ=DAILY").expect("parse");
        assert_eq!(parsed.freq, Frequency::Daily);
    }

    #[test]
    fn ends_are_described() {
        let start = date(2026, 9, 1);
        let mut weekly = weekly_on(start);
        weekly.end = RecurrenceEnd::OnDate(date(2026, 12, 1));
        assert_eq!(weekly.describe(start), "Weekly on Tuesday, until Dec 1, 2026");

        weekly.end = RecurrenceEnd::AfterCount(13);
        assert_eq!(weekly.describe(start), "Weekly on Tuesday, 13 times");

        weekly.end = RecurrenceEnd::AfterCount(1);
        assert_eq!(weekly.describe(start), "Weekly on Tuesday, 1 time");
    }

    #[test]
    fn custom_weekly_rule_with_multiple_days_is_not_collapsed_to_a_preset_label() {
        let start = date(2026, 9, 1);
        let recurrence = Recurrence {
            freq: Frequency::Weekly,
            interval: 2,
            by_day: vec![Weekday::Mon, Weekday::Wed],
            monthly_ordinal: None,
            end: RecurrenceEnd::Never,
        };
        assert_eq!(recurrence.describe(start), "Every 2 weeks on Monday, Wednesday");
    }
}
