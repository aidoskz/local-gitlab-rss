//! Hourly digest: who pushed how many commits, with short commit titles,
//! plus a day report at local midnight with commit counts per person.

use std::collections::{BTreeMap, HashMap, HashSet};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::gitlab::{Gitlab, RawCommit};

/// How far back commits are re-read each run, so commits pushed late
/// (made at 13:50, pushed at 15:10) still get reported once.
const LOOKBACK_SECS: i64 = 48 * 3600;
/// Reported commits are remembered this long, so a copy cherry-picked into a
/// release branch days later is still recognised.
const SEEN_TTL_SECS: i64 = 30 * 86400;
/// Telegram allows 4096 characters per message; leave room for entities.
const MESSAGE_LIMIT: usize = 4000;
/// A run a few minutes after midnight still closes the previous day.
const DAY_SLACK_SECS: i64 = 300;

/// Commit counts per person for one local day (days since 1970-01-01, local time).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DayCounts {
    #[serde(default)]
    pub day: i64,
    #[serde(default)]
    pub counts: BTreeMap<String, usize>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// End of the last reported window (unix seconds), 0 before the first run.
    #[serde(default)]
    pub last_at: i64,
    /// Keys of commits already reported (see `Commit::key`) -> when first reported.
    #[serde(default)]
    pub seen: BTreeMap<String, i64>,
    /// Author e-mail (lowercase) or author name -> name shown in the report.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    /// Commits reported so far today, by person.
    #[serde(default)]
    pub today: DayCounts,
    /// Finished days whose report is not sent yet (kept until Telegram accepts it).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finished_days: Vec<DayCounts>,
}

impl State {
    pub fn display_name(&self, name: &str, email: &str) -> String {
        self.aliases
            .get(&email.to_lowercase())
            .or_else(|| self.aliases.get(name))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }
}

pub struct Options {
    pub utc_offset_hours: i64,
    pub titles_per_person: usize,
    pub title_len: usize,
    pub exclude_authors: Option<Regex>,
}

impl Options {
    pub fn from_env() -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let num = |k: &str, default: i64| -> Result<i64> {
            match var(k) {
                Some(v) => v.parse().map_err(|_| format!("{k}: not a number: {v}").into()),
                None => Ok(default),
            }
        };
        let exclude_authors = match var("DIGEST_EXCLUDE") {
            Some(p) => Some(Regex::new(&p).map_err(|e| format!("DIGEST_EXCLUDE: {e}"))?),
            None => None,
        };
        Ok(Options {
            utc_offset_hours: num("DIGEST_UTC_OFFSET", 5)?,
            titles_per_person: num("DIGEST_TITLES", 3)?.max(0) as usize,
            title_len: num("DIGEST_TITLE_LEN", 60)?.max(10) as usize,
            exclude_authors,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub author: String,
    pub email: String,
    pub title: String,
    pub authored: i64,
    pub committed: i64,
}

impl Commit {
    /// Rebases and cherry-picks (e.g. into release branches) give a commit a new
    /// sha but keep author, author date and title, so the copies share one key.
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.email.to_lowercase(), self.authored, self.title)
    }
}

pub struct Report<'a> {
    pub group: &'a str,
    pub from: i64,
    pub to: i64,
    pub commits: &'a [Commit],
    pub merged_mrs: usize,
    pub releases: &'a [String],
}

/// Message text (Telegram HTML), or None when nothing happened.
pub fn render(r: &Report, opts: &Options) -> Option<String> {
    if r.commits.is_empty() && r.merged_mrs == 0 && r.releases.is_empty() {
        return None;
    }
    let mut by_author: HashMap<&str, Vec<&Commit>> = HashMap::new();
    for c in r.commits {
        by_author.entry(c.author.as_str()).or_default().push(c);
    }
    let mut authors: Vec<(&str, Vec<&Commit>)> = by_author.into_iter().collect();
    authors.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

    let mut head = format!(
        "<b>{} · {}</b> · {}",
        html_escape(r.group),
        window_label(r.from, r.to, opts.utc_offset_hours),
        plural(r.commits.len(), "commit", "commits")
    );
    if r.merged_mrs > 0 {
        head += &format!(" · {} merged", plural(r.merged_mrs, "MR", "MRs"));
    }
    let mut blocks = vec![head];
    for (author, mut commits) in authors {
        commits.sort_by_key(|c| c.committed);
        let mut block = format!("<b>{}</b> — {}", html_escape(author), commits.len());
        for c in commits.iter().take(opts.titles_per_person) {
            block += &format!("\n  {}", html_escape(&truncate(&c.title, opts.title_len)));
        }
        if commits.len() > opts.titles_per_person {
            block += &format!("\n  …and {} more", commits.len() - opts.titles_per_person);
        }
        blocks.push(block);
    }
    if !r.releases.is_empty() {
        blocks.push(format!("Releases: {}", html_escape(&r.releases.join(", "))));
    }
    Some(blocks.join("\n\n"))
}

/// Split on blank lines so a long report goes out as several messages,
/// never cutting a person's block in half (unless one block alone is too long).
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for block in text.split("\n\n") {
        let extra = if cur.is_empty() { block.len() } else { block.len() + 2 };
        if !cur.is_empty() && cur.len() + extra > limit {
            parts.push(std::mem::take(&mut cur));
        }
        if block.len() > limit {
            let mut chunk = String::new();
            for line in block.lines() {
                if !chunk.is_empty() && chunk.len() + line.len() + 1 > limit {
                    parts.push(std::mem::take(&mut chunk));
                }
                if !chunk.is_empty() {
                    chunk.push('\n');
                }
                chunk.push_str(line);
            }
            cur = chunk;
            continue;
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(block);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// One digest run: report what happened since the last run.
/// With `send` = None the report is only printed and the state is left alone.
pub fn run_once(
    gl: &Gitlab,
    state: &mut State,
    opts: &Options,
    now: i64,
    preview_from: Option<i64>,
    send: Option<&dyn Fn(&str) -> Result<()>>,
) -> Result<()> {
    let first_run = state.last_at == 0;
    let from = preview_from.unwrap_or(if first_run { now - 3600 } else { state.last_at });
    let since = from.min(now - LOOKBACK_SECS);

    let group = gl.group_name()?;
    let projects = gl.projects(since)?;
    let seen: HashSet<&str> = if preview_from.is_some() {
        HashSet::new()
    } else {
        state.seen.keys().map(String::as_str).collect()
    };

    let mut fresh: Vec<Commit> = Vec::new();
    let mut seed: Vec<String> = Vec::new();
    let mut batch: HashSet<String> = HashSet::new();
    let mut releases: Vec<String> = Vec::new();
    for p in &projects {
        for raw in gl.commits(p, since, now)? {
            if let Some(c) = to_commit(&raw, state, opts) {
                let key = c.key();
                if seen.contains(key.as_str()) || !batch.insert(key.clone()) {
                    continue;
                }
                // Before the window: on the first run or in a preview, only mark as seen.
                if (first_run || preview_from.is_some()) && c.committed < from {
                    seed.push(key);
                } else {
                    fresh.push(c);
                }
            }
        }
        for tag in gl.tags(p, from, now)? {
            releases.push(format!("{} {}", p.name, tag));
        }
    }
    let merged_mrs = gl.merged_mrs(&projects, from, now)?;

    let report = Report { group: &group, from, to: now, commits: &fresh, merged_mrs, releases: &releases };
    let text = render(&report, opts);
    let Some(send) = send else {
        println!("{}", text.as_deref().unwrap_or("Nothing to report."));
        return Ok(());
    };
    if let Some(text) = &text {
        for part in split_message(text, MESSAGE_LIMIT) {
            send(&part)?;
        }
    }
    remember(state, seed.into_iter().chain(fresh.iter().map(Commit::key)), now);
    state.last_at = now;

    let authors: Vec<String> = fresh.iter().map(|c| c.author.clone()).collect();
    record_day(state, &authors, now, opts.utc_offset_hours);
    while let Some(day) = state.finished_days.first() {
        if let Some(text) = render_day(&group, day) {
            for part in split_message(&text, MESSAGE_LIMIT) {
                send(&part)?;
            }
        }
        state.finished_days.remove(0);
    }
    Ok(())
}

fn remember(state: &mut State, keys: impl Iterator<Item = String>, now: i64) {
    for key in keys {
        state.seen.entry(key).or_insert(now);
    }
    state.seen.retain(|_, at| *at >= now - SEEN_TTL_SECS);
}

/// Add reported commits to today's counts and close the day at local midnight.
/// Commits count for the day they were reported in, so the day report is the
/// sum of that day's hourly reports.
pub fn record_day(state: &mut State, authors: &[String], now: i64, utc_offset_hours: i64) {
    let day = local_day(now - DAY_SLACK_SECS, utc_offset_hours);
    if state.today.day != day {
        close_today(state, day);
    }
    for a in authors {
        *state.today.counts.entry(a.clone()).or_default() += 1;
    }
    let current = local_day(now, utc_offset_hours);
    if current != day {
        close_today(state, current);
    }
}

fn close_today(state: &mut State, next_day: i64) {
    let done = std::mem::replace(&mut state.today, DayCounts { day: next_day, counts: BTreeMap::new() });
    if done.day != 0 && !done.counts.is_empty() {
        state.finished_days.push(done);
    }
}

fn local_day(ts: i64, utc_offset_hours: i64) -> i64 {
    (ts + utc_offset_hours * 3600).div_euclid(86400)
}

/// Day report: commit count per person, sorted by name.
pub fn render_day(group: &str, day: &DayCounts) -> Option<String> {
    let total: usize = day.counts.values().sum();
    if total == 0 {
        return None;
    }
    let (y, m, d) = civil_from_days(day.day);
    let mut people: Vec<(&String, &usize)> = day.counts.iter().filter(|(_, n)| **n > 0).collect();
    people.sort_by_key(|(name, _)| name.to_lowercase());
    let mut text = format!(
        "<b>{} · day {d:02}.{m:02}.{y}</b> · {} · {}",
        html_escape(group),
        plural(total, "commit", "commits"),
        plural(people.len(), "person", "people")
    );
    text += "\n";
    for (name, n) in people {
        text += &format!("\n{} — {n}", html_escape(name));
    }
    Some(text)
}

fn to_commit(raw: &RawCommit, state: &State, opts: &Options) -> Option<Commit> {
    if raw.merge {
        return None;
    }
    if let Some(re) = &opts.exclude_authors {
        if re.is_match(&raw.author_name) || re.is_match(&raw.author_email) {
            return None;
        }
    }
    Some(Commit {
        author: state.display_name(&raw.author_name, &raw.author_email),
        email: raw.author_email.clone(),
        title: raw.title.clone(),
        authored: raw.authored,
        committed: raw.committed,
    })
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// "13:00–14:00" within one day, "06.11 22:00 – 07.11 13:00" across days.
pub fn window_label(from: i64, to: i64, utc_offset_hours: i64) -> String {
    let local = |ts: i64| {
        let t = ts + utc_offset_hours * 3600;
        let (_, m, d) = civil_from_days(t.div_euclid(86400));
        let secs = t.rem_euclid(86400);
        ((m, d), format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60))
    };
    let ((fm, fd), ft) = local(from);
    let ((tm, td), tt) = local(to);
    if (fm, fd) == (tm, td) {
        format!("{ft}–{tt}")
    } else {
        format!("{fd:02}.{fm:02} {ft} – {td:02}.{tm:02} {tt}")
    }
}

/// Parse GitLab timestamps: "2026-11-06T21:02:13.000+05:00" or "...Z".
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if s.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let days = days_from_civil(num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let secs = num(11, 13)? * 3600 + num(14, 16)? * 60 + num(17, 19)?;
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hh: i64 = rest.get(1..3)?.parse().ok()?;
            let mm: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (hh * 3600 + mm * 60)
        }
    };
    Some(days * 86400 + secs - offset)
}

/// "2026-11-06T16:02:13Z", the form the GitLab API accepts in query parameters.
pub fn format_utc(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    let s = ts.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

pub fn format_date(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    format!("{y:04}-{m:02}-{d:02}")
}

// Howard Hinnant's civil calendar algorithms (proleptic Gregorian, UTC).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_2026_11_06_16_02_13Z: i64 = 1793980933;
    const T_2026_11_06_18_00Z: i64 = 1793988000; // 23:00 in UTC+5
    const T_2026_11_07_08_00Z: i64 = 1794038400; // 13:00 in UTC+5

    fn opts() -> Options {
        Options { utc_offset_hours: 5, titles_per_person: 2, title_len: 20, exclude_authors: None }
    }

    fn commit(author: &str, title: &str, committed: i64) -> Commit {
        Commit {
            author: author.into(),
            email: format!("{}@example.com", author.to_lowercase().replace(' ', ".")),
            title: title.into(),
            authored: committed,
            committed,
        }
    }

    #[test]
    fn parses_gitlab_times() {
        assert_eq!(parse_time("2026-11-06T21:02:13.000+05:00"), Some(T_2026_11_06_16_02_13Z));
        assert_eq!(parse_time("2026-11-06T16:02:13Z"), Some(T_2026_11_06_16_02_13Z));
        assert_eq!(parse_time("2026-11-06T16:02:13.123456Z"), Some(T_2026_11_06_16_02_13Z));
        assert_eq!(parse_time("2026-11-06T14:02:13-02:00"), Some(T_2026_11_06_16_02_13Z));
        assert_eq!(parse_time("1999-12-31T23:59:59Z"), Some(946684799));
        assert_eq!(parse_time("2026-11-06"), None);
        assert_eq!(parse_time("garbage, not a date at all"), None);
    }

    #[test]
    fn formats_utc_round_trip() {
        assert_eq!(format_utc(T_2026_11_06_16_02_13Z), "2026-11-06T16:02:13Z");
        assert_eq!(format_utc(946684799), "1999-12-31T23:59:59Z");
        assert_eq!(format_date(T_2026_11_06_16_02_13Z), "2026-11-06");
    }

    #[test]
    fn labels_windows_in_local_time() {
        let to = T_2026_11_07_08_00Z;
        assert_eq!(window_label(to - 3600, to, 5), "12:00–13:00");
        assert_eq!(window_label(T_2026_11_06_18_00Z, to, 5), "06.11 23:00 – 07.11 13:00");
    }

    #[test]
    fn nothing_to_report_is_none() {
        let r = Report { group: "Team", from: 0, to: 3600, commits: &[], merged_mrs: 0, releases: &[] };
        assert_eq!(render(&r, &opts()), None);
    }

    #[test]
    fn renders_people_by_commit_count_with_short_titles() {
        let to = T_2026_11_07_08_00Z;
        let commits = vec![
            commit("John Smith", "feat(ui): dashboard", to - 100),
            commit("Jane Doe", "fix(forms): кнопка «Сохранить» — проверки", to - 3000),
            commit("Jane Doe", "fix(a): one", to - 2000),
            commit("Jane Doe", "fix(b): <two> & co", to - 1000),
        ];
        let releases = vec!["backend 1.4.0".to_string()];
        let r = Report { group: "Team", from: to - 3600, to, commits: &commits, merged_mrs: 1, releases: &releases };
        let text = render(&r, &opts()).unwrap();
        assert_eq!(
            text,
            "<b>Team · 12:00–13:00</b> · 4 commits · 1 MR merged\n\n\
             <b>Jane Doe</b> — 3\n  fix(forms): кнопка…\n  fix(a): one\n  …and 1 more\n\n\
             <b>John Smith</b> — 1\n  feat(ui): dashboard\n\n\
             Releases: backend 1.4.0"
        );
    }

    #[test]
    fn escapes_html_in_titles() {
        let to = T_2026_11_07_08_00Z;
        let commits = vec![commit("A", "fix: <b> & co", to)];
        let r = Report { group: "G", from: to - 60, to, commits: &commits, merged_mrs: 0, releases: &[] };
        assert!(render(&r, &opts()).unwrap().contains("fix: &lt;b&gt; &amp; co"));
    }

    #[test]
    fn rebased_or_cherry_picked_copies_share_a_key() {
        let a = commit("A", "fix: x", 100);
        let mut copy = a.clone();
        copy.committed = 500;
        assert_eq!(a.key(), copy.key());
        assert_ne!(a.key(), commit("A", "fix: y", 100).key());
    }

    #[test]
    fn aliases_by_email_then_name() {
        let mut s = State::default();
        s.aliases.insert("jane.doe@personal.example".into(), "Jane Doe".into());
        s.aliases.insert("jsmith".into(), "John Smith".into());
        assert_eq!(s.display_name("J. Doe", "Jane.Doe@Personal.Example"), "Jane Doe");
        assert_eq!(s.display_name("jsmith", "j.smith@example.com"), "John Smith");
        assert_eq!(s.display_name("Alex K", "a@example.com"), "Alex K");
    }

    #[test]
    fn remembers_reported_commits_for_thirty_days() {
        let mut s = State::default();
        let now = T_2026_11_07_08_00Z;
        remember(&mut s, ["old".to_string()].into_iter(), now - 31 * 86400);
        remember(&mut s, ["kept".to_string()].into_iter(), now - 29 * 86400);
        remember(&mut s, ["kept".to_string(), "new".to_string()].into_iter(), now);
        assert_eq!(s.seen.keys().collect::<Vec<_>>(), vec!["kept", "new"]);
        assert_eq!(s.seen["kept"], now - 29 * 86400);
    }

    fn people(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn day_counts_add_up_and_close_at_midnight() {
        let mut s = State::default();
        let midnight = T_2026_11_06_18_00Z + 3600; // 07.11 00:00 in UTC+5
        record_day(&mut s, &people(&["B", "A", "B"]), midnight - 3600, 5); // 23:00 run
        assert!(s.finished_days.is_empty());
        record_day(&mut s, &people(&["A"]), midnight + 5, 5); // 00:00 run closes 06.11
        assert_eq!(s.finished_days.len(), 1);
        let day = &s.finished_days[0];
        assert_eq!(day.counts.get("A"), Some(&2));
        assert_eq!(day.counts.get("B"), Some(&2));
        assert!(s.today.counts.is_empty());
        assert_eq!(
            render_day("Team", day).unwrap(),
            "<b>Team · day 06.11.2026</b> · 4 commits · 2 people\n\nA — 2\nB — 2"
        );
        record_day(&mut s, &people(&["C"]), midnight + 3605, 5); // 01:00 run starts 07.11
        assert_eq!(s.finished_days.len(), 1);
        assert_eq!(s.today.counts.get("C"), Some(&1));
    }

    #[test]
    fn day_report_sorts_by_name_not_count() {
        let mut counts = BTreeMap::new();
        counts.insert("zoe Young".to_string(), 30);
        counts.insert("Alice Brown".to_string(), 2);
        counts.insert("Bob Green".to_string(), 9);
        let day = DayCounts { day: 20398, counts };
        let text = render_day("G", &day).unwrap();
        let body: Vec<&str> = text.lines().skip(2).collect();
        assert_eq!(body, vec!["Alice Brown — 2", "Bob Green — 9", "zoe Young — 30"]);
    }

    #[test]
    fn missed_midnight_is_reported_on_the_next_run() {
        let mut s = State::default();
        let midnight = T_2026_11_06_18_00Z + 3600;
        record_day(&mut s, &people(&["A"]), midnight - 7200, 5); // 22:00
        record_day(&mut s, &people(&["B"]), midnight + 9 * 3600, 5); // VPN back at 09:00
        assert_eq!(s.finished_days.len(), 1);
        assert_eq!(s.finished_days[0].counts.get("A"), Some(&1));
        assert_eq!(s.today.counts.get("B"), Some(&1));
        assert_eq!(render_day("G", &DayCounts::default()), None);
    }

    #[test]
    fn splits_long_reports_on_person_blocks() {
        let text = format!("{}\n\n{}\n\n{}", "a".repeat(30), "b".repeat(30), "c".repeat(30));
        assert_eq!(split_message(&text, 70), vec![format!("{}\n\n{}", "a".repeat(30), "b".repeat(30)), "c".repeat(30)]);
        assert_eq!(split_message("short", 70), vec!["short"]);
        let long_block = format!("{}\n{}", "x".repeat(40), "y".repeat(40));
        assert_eq!(split_message(&long_block, 50), vec!["x".repeat(40), "y".repeat(40)]);
    }
}
