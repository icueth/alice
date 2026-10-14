//! Numbers and dates in words: Jev reads "two weeks ago" and "importance high" better than
//! digits. Only metadata is verbalised; memory text is sent as written.

use super::JudgeMemory;
use chrono::{DateTime, Utc};

/// Characters of a memory body sent to a judge.
pub const BODY_CHARS: usize = 700;

const WORDS: [&str; 21] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

pub fn number(n: u64) -> String {
    match n {
        0..=20 => WORDS[n as usize].into(),
        _ if n < 100 && n.is_multiple_of(10) => TENS[(n / 10) as usize].into(),
        _ => n.to_string(),
    }
}

fn ago(n: i64, one: &str, unit: &str) -> String {
    if n <= 1 {
        format!("{one} {unit} ago")
    } else {
        format!("{} {unit}s ago", number(n as u64))
    }
}

pub fn age(created_at: &str, now: DateTime<Utc>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(created_at) else {
        return "at an unknown time".into();
    };
    let days = (now - t.with_timezone(&Utc)).num_days();
    match days {
        ..=0 => "today".into(),
        1 => "yesterday".into(),
        2..=6 => format!("{} days ago", number(days as u64)),
        7..=13 => "a week ago".into(),
        14..=29 => ago(days / 7, "a", "week"),
        30..=59 => "a month ago".into(),
        60..=364 => ago(days / 30, "a", "month"),
        365..=729 => "a year ago".into(),
        _ => ago(days / 365, "a", "year"),
    }
}

pub fn importance(i: u8) -> &'static str {
    match i {
        0 | 1 => "importance minor",
        2 => "importance low",
        3 => "importance normal",
        4 => "importance high",
        _ => "importance critical",
    }
}

pub fn memory_block(label: &str, m: &JudgeMemory, now: DateTime<Utc>) -> String {
    let body = crate::normalize::truncate_chars(&m.body, BODY_CHARS);
    format!(
        "{label} (written {}, kind {}, {}, project {}):\nTitle: {}\n{}\n",
        age(&m.created_at, now),
        m.kind.as_str(),
        importance(m.importance),
        m.project,
        crate::normalize::truncate_chars(&m.title, 200),
        body
    )
}

pub fn kind_state(m: &JudgeMemory, now: DateTime<Utc>) -> String {
    format!(
        "Which kind of memory is this?\n\n{}",
        memory_block("Memory", m, now)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ages_and_numbers_are_words() {
        let now = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let at = |d: i64| (now - chrono::Duration::days(d)).to_rfc3339();
        assert_eq!(age(&at(0), now), "today");
        assert_eq!(age(&at(1), now), "yesterday");
        assert_eq!(age(&at(3), now), "three days ago");
        assert_eq!(age(&at(8), now), "a week ago");
        assert_eq!(age(&at(15), now), "two weeks ago");
        assert_eq!(age(&at(40), now), "a month ago");
        assert_eq!(age(&at(200), now), "six months ago");
        assert_eq!(age(&at(400), now), "a year ago");
        assert_eq!(age(&at(1_100), now), "three years ago");
        assert_eq!(age("not a date", now), "at an unknown time");
        assert_eq!(number(12), "twelve");
        assert_eq!(number(40), "forty");
        assert_eq!(number(137), "137");
        assert_eq!(importance(4), "importance high");
    }

    #[test]
    fn memory_blocks_have_no_digits_from_metadata_and_bound_the_body() {
        let now = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let m = JudgeMemory {
            id: "01X".into(),
            project: "app".into(),
            kind: crate::model::Kind::Decision,
            title: "Use WAL".into(),
            body: "x".repeat(5_000),
            importance: 5,
            created_at: (now - chrono::Duration::days(14)).to_rfc3339(),
            similarity: None,
        };
        let b = memory_block("Existing memory 1", &m, now);
        assert!(
            b.starts_with("Existing memory 1 (written two weeks ago, kind decision, importance critical, project app):"),
            "{b}"
        );
        assert!(b.chars().count() < BODY_CHARS + 200);
    }
}
