use std::collections::HashMap;

use anyhow::Result;
use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::db::{Db, Group, GroupBy};

/// Days shown in the activity grid (12 weeks).
pub const DAYS: usize = 84;

/// Everything the stats page shows.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Bytes actually transferred, including paused and failed downloads.
    pub lifetime_bytes: u64,
    pub today_bytes: u64,
    pub week_bytes: u64,
    pub month_bytes: u64,
    pub completed: u64,
    pub failed: u64,
    /// Bytes/s over the time completed downloads spent running.
    pub avg_speed: f64,
    pub peak_speed: f64,
    pub largest: Option<Group>,
    /// The last `DAYS` days, oldest first, with gaps filled with zero.
    pub days: Vec<Day>,
    /// Bytes by local hour of day, 0-23.
    pub hours: [u64; 24],
    pub categories: Vec<Group>,
    pub hosts: Vec<Group>,
    /// First day with any traffic, `YYYY-MM-DD`.
    pub since: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Day {
    pub date: String,
    pub bytes: u64,
}

pub fn compute(db: &Db, now: NaiveDateTime) -> Result<Stats> {
    let traffic = db.traffic()?;
    let mut by_day: HashMap<&str, u64> = HashMap::new();
    let mut hours = [0u64; 24];
    for (hour, bytes) in &traffic {
        // Keys look like 2026-10-27T17.
        let (date, h) = hour.split_once('T').unwrap_or((hour, "0"));
        *by_day.entry(date).or_default() += bytes;
        if let Ok(h) = h.parse::<usize>() {
            hours[h.min(23)] += bytes;
        }
    }

    let today = now.date();
    let days: Vec<Day> = (0..DAYS)
        .rev()
        .map(|back| {
            let date = (today - Duration::days(back as i64)).format("%Y-%m-%d").to_string();
            let bytes = by_day.get(date.as_str()).copied().unwrap_or(0);
            Day { date, bytes }
        })
        .collect();
    let last = |n: usize| days[DAYS - n..].iter().map(|d| d.bytes).sum::<u64>();

    let totals = db.history_totals()?;
    Ok(Stats {
        lifetime_bytes: traffic.iter().map(|(_, b)| b).sum(),
        today_bytes: last(1),
        week_bytes: last(7),
        month_bytes: last(30),
        completed: totals.completed,
        failed: totals.failed,
        avg_speed: if totals.timed_ms > 0 { totals.timed_bytes as f64 * 1000.0 / totals.timed_ms as f64 } else { 0.0 },
        peak_speed: totals.peak_speed,
        largest: db.largest()?,
        days,
        hours,
        categories: db.history_groups(GroupBy::Category, 8)?,
        hosts: db.history_groups(GroupBy::Host, 6)?,
        since: traffic
            .first()
            .and_then(|(h, _)| NaiveDate::parse_from_str(&h[..10.min(h.len())], "%Y-%m-%d").ok())
            .map(|d| d.format("%Y-%m-%d").to_string()),
    })
}

/// The traffic key for a local time: `2026-10-27T17`.
pub fn hour_key(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::HistoryEntry;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn entry(name: &str, host: &str, category: &str, bytes: u64, ms: u64, ok: bool) -> HistoryEntry {
        HistoryEntry {
            download_id: 1,
            name: name.into(),
            host: host.into(),
            category: category.into(),
            bytes,
            active_ms: ms,
            peak_speed: bytes as f64 / 2.0,
            ok,
        }
    }

    #[test]
    fn empty_database_gives_zeros() {
        let db = Db::in_memory().unwrap();
        let s = compute(&db, at("2026-10-27 12:00")).unwrap();
        assert_eq!(s.lifetime_bytes, 0);
        assert_eq!(s.days.len(), DAYS);
        assert_eq!(s.days.last().unwrap().date, "2026-10-27");
        assert!(s.largest.is_none() && s.since.is_none());
        assert_eq!(s.avg_speed, 0.0);
    }

    #[test]
    fn sums_traffic_by_day_window_and_hour() {
        let db = Db::in_memory().unwrap();
        db.add_traffic("2026-10-27T09", 100).unwrap();
        db.add_traffic("2026-10-27T09", 50).unwrap();
        db.add_traffic("2026-10-25T23", 1000).unwrap();
        db.add_traffic("2026-10-01T09", 7).unwrap();
        db.add_traffic("2025-01-01T00", 3).unwrap();

        let s = compute(&db, at("2026-10-27 18:30")).unwrap();
        assert_eq!(s.lifetime_bytes, 1160);
        assert_eq!(s.today_bytes, 150);
        assert_eq!(s.week_bytes, 1150);
        assert_eq!(s.month_bytes, 1157, "Oct 1 is 26 days back, inside 30");
        assert_eq!(s.hours[9], 157);
        assert_eq!(s.hours[23], 1000);
        assert_eq!(s.since.as_deref(), Some("2025-01-01"));
        let day = |d: &str| s.days.iter().find(|x| x.date == d).unwrap().bytes;
        assert_eq!(day("2026-10-26"), 0, "gaps are zero-filled");
        assert_eq!(day("2026-10-25"), 1000);
    }

    #[test]
    fn history_drives_counts_speeds_and_groups() {
        let db = Db::in_memory().unwrap();
        db.add_history(&entry("a.iso", "x.com", "Programs", 6000, 2000, true)).unwrap();
        db.add_history(&entry("b.mp4", "y.com", "Video", 2000, 2000, true)).unwrap();
        db.add_history(&entry("c.mp4", "y.com", "Video", 1000, 0, true)).unwrap();
        db.add_history(&entry("d.zip", "z.com", "Archives", 500, 100, false)).unwrap();

        let s = compute(&db, at("2026-10-27 12:00")).unwrap();
        assert_eq!((s.completed, s.failed), (3, 1));
        // (6000 + 2000) bytes over 4 s; the untimed one is left out.
        assert_eq!(s.avg_speed, 2000.0);
        assert_eq!(s.peak_speed, 3000.0);
        assert_eq!(s.largest.unwrap().name, "a.iso");
        assert_eq!(s.categories[0].name, "Programs");
        assert_eq!((s.categories[1].name.as_str(), s.categories[1].count), ("Video", 2));
        assert!(s.categories.iter().all(|c| c.name != "Archives"), "failed downloads aren't grouped");
        assert_eq!(s.hosts[0].name, "x.com");
    }

    #[test]
    fn hour_keys() {
        assert_eq!(hour_key(at("2026-03-04 05:59")), "2026-03-04T05");
    }
}
