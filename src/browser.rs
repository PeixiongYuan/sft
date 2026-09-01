use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Local,
    Remote,
}

impl Pane {
    pub fn other(self) -> Self {
        match self {
            Self::Local => Self::Remote,
            Self::Remote => Self::Local,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    DateDesc,
    DateAsc,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::DateDesc,
            Self::DateDesc => Self::DateAsc,
            Self::DateAsc => Self::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::DateDesc => "date desc",
            Self::DateAsc => "date asc",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<i64>,
}

pub fn list_local(path: &Path, show_hidden: bool, sort: SortMode) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        let metadata = entry.metadata()?;
        let is_dir = metadata.is_dir();
        let modified = metadata.modified().ok().and_then(system_time_to_epoch);
        entries.push(Entry {
            name,
            is_dir,
            size: (!is_dir).then_some(metadata.len()),
            modified,
        });
    }
    sort_entries(&mut entries, sort);
    Ok(entries)
}

pub fn sort_entries(entries: &mut [Entry], sort: SortMode) {
    entries.sort_by(|a, b| {
        let dir_order = b.is_dir.cmp(&a.is_dir);
        if dir_order != Ordering::Equal {
            return dir_order;
        }

        match sort {
            SortMode::Name => a
                .name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase()),
            SortMode::DateDesc => b.modified.unwrap_or(0).cmp(&a.modified.unwrap_or(0)),
            SortMode::DateAsc => a
                .modified
                .unwrap_or(i64::MAX)
                .cmp(&b.modified.unwrap_or(i64::MAX)),
        }
    });
}

pub fn filter_entries(entries: &[Entry], query: &str) -> Vec<Entry> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return entries.to_vec();
    }

    entries
        .iter()
        .filter(|entry| entry.name.to_ascii_lowercase().contains(&query))
        .cloned()
        .collect()
}

pub fn parse_ls_output(output: &str, show_hidden: bool, sort: SortMode) -> Vec<Entry> {
    let mut entries = Vec::new();
    for line in output.lines() {
        if let Some(entry) = parse_long_listing_line(line, show_hidden) {
            entries.push(entry);
        }
    }
    sort_entries(&mut entries, sort);
    entries
}

fn parse_long_listing_line(line: &str, show_hidden: bool) -> Option<Entry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with("total ") {
        return None;
    }

    let mut parts = Vec::with_capacity(9);
    let mut rest = line;
    for _ in 0..8 {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        parts.push(&rest[..end]);
        rest = &rest[end..];
    }
    rest = rest.trim_start();
    if !rest.is_empty() {
        parts.push(rest);
    }
    if parts.len() < 9 {
        return None;
    }

    let permissions = parts[0];
    let is_dir = permissions.starts_with('d');
    let raw_name = parts[8];
    let name = listing_name(raw_name);
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    if !show_hidden && name.starts_with('.') {
        return None;
    }

    Some(Entry {
        name: name.to_string(),
        is_dir,
        size: (!is_dir).then_some(parse_human_size(parts[4])),
        modified: parse_ls_date(parts[5], parts[6], parts[7]),
    })
}

fn listing_name(raw_name: &str) -> String {
    let name = raw_name
        .split_once(" -> ")
        .map(|(name, _)| name)
        .unwrap_or(raw_name)
        .trim();
    let without_trailing_slash = name.trim_end_matches('/');
    without_trailing_slash
        .rsplit('/')
        .next()
        .filter(|leaf| !leaf.is_empty())
        .unwrap_or(without_trailing_slash)
        .to_string()
}

pub fn parent_remote_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "/" {
        return "/".to_string();
    }

    let path = trimmed.trim_end_matches('/');
    if path.is_empty() || path == "." {
        return "..".to_string();
    }
    if let Some(pos) = path.rfind('/') {
        if pos == 0 {
            "/".to_string()
        } else {
            path[..pos].to_string()
        }
    } else {
        "..".to_string()
    }
}

pub fn join_remote_path(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{name}")
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

pub fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_relative_time(ts: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let diff = now - ts;
    if diff < 0 {
        return format_short_date(ts);
    }
    if diff < 60 {
        return "now".to_string();
    }
    if diff < 3600 {
        return format!("{}m", diff / 60);
    }
    if diff < 86400 {
        return format!("{}h", diff / 3600);
    }
    if diff < 86400 * 30 {
        return format!("{}d", diff / 86400);
    }
    format_short_date(ts)
}

fn format_short_date(ts: i64) -> String {
    let year = epoch_to_year(ts);
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let year_start = approximate_epoch(year, 0, 1, 0, 0);
    let day_of_year = ((ts - year_start) / 86400).max(0) as usize;
    let feb = if is_leap_year(year) { 29 } else { 28 };
    let lengths = [31, feb, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    let mut remaining = day_of_year;
    for (idx, len) in lengths.iter().enumerate() {
        month = idx;
        if remaining < *len {
            break;
        }
        remaining -= *len;
    }
    format!("{} {}", months[month.min(11)], remaining + 1)
}

fn parse_human_size(value: &str) -> u64 {
    let value = value.trim();
    if value.is_empty() {
        return 0;
    }
    let last = value.as_bytes()[value.len() - 1];
    let multiplier = match last {
        b'K' => 1024,
        b'M' => 1024 * 1024,
        b'G' => 1024 * 1024 * 1024,
        b'T' => 1024_u64 * 1024 * 1024 * 1024,
        _ => 1,
    };
    let number = if multiplier > 1 {
        &value[..value.len() - 1]
    } else {
        value
    };
    (number.parse::<f64>().unwrap_or(0.0) * multiplier as f64) as u64
}

fn parse_ls_date(month: &str, day: &str, time_or_year: &str) -> Option<i64> {
    let month = match month {
        "Jan" => 0,
        "Feb" => 1,
        "Mar" => 2,
        "Apr" => 3,
        "May" => 4,
        "Jun" => 5,
        "Jul" => 6,
        "Aug" => 7,
        "Sep" => 8,
        "Oct" => 9,
        "Nov" => 10,
        "Dec" => 11,
        _ => return None,
    };
    let day = day.parse::<i64>().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }

    if time_or_year.contains(':') {
        let mut parts = time_or_year.splitn(2, ':');
        let hour = parts.next()?.parse().ok()?;
        let minute = parts.next()?.parse().ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let mut year = epoch_to_year(now);
        let candidate = approximate_epoch(year, month, day, hour, minute);
        if candidate > now + 86400 {
            year -= 1;
        }
        Some(approximate_epoch(year, month, day, hour, minute))
    } else {
        let year = time_or_year.parse().ok()?;
        Some(approximate_epoch(year, month, day, 0, 0))
    }
}

fn approximate_epoch(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> i64 {
    let y = year - 1970;
    let mut days = y * 365 + (y + 1) / 4;
    let month_days = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    days += month_days[month as usize];
    if month > 1 && is_leap_year(year) {
        days += 1;
    }
    days += day - 1;
    days * 86400 + hour * 3600 + minute * 60
}

fn epoch_to_year(ts: i64) -> i64 {
    let mut year = 1970 + ts / 31_557_600;
    if approximate_epoch(year, 0, 1, 0, 0) > ts {
        year -= 1;
    } else if approximate_epoch(year + 1, 0, 1, 0, 0) <= ts {
        year += 1;
    }
    year
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn system_time_to_epoch(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

#[allow(dead_code)]
pub fn clean_local_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ls_output_with_spaces_and_hidden_filter() {
        let output = "\
total 8
drwxr-xr-x  2 me staff  64 May 28 10:00 docs
-rw-r--r--  1 me staff 1.5K May 28 10:01 report final.txt
-rw-r--r--  1 me staff    5 May 28 10:02 .secret
";
        let entries = parse_ls_output(output, false, SortMode::Name);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "docs");
        assert_eq!(entries[1].name, "report final.txt");
        assert_eq!(entries[1].size, Some(1536));
    }

    #[test]
    fn parses_sftp_listing_with_absolute_names() {
        let output = "\
drwxr-xr-x    ? user group       4096 May 28 10:00 /remote/base/../
drwxr-xr-x    ? user group       4096 May 28 10:00 /remote/base/./
drwxr-xr-x    ? user group       4096 May 28 10:00 /remote/base/.cache/
drwxr-xr-x    ? user group       4096 May 28 10:00 /remote/base/data/
-rw-r--r--    ? user group        128 May 28 10:01 /remote/base/report.txt
";
        let entries = parse_ls_output(output, false, SortMode::Name);

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["data", "report.txt"]
        );
    }

    #[test]
    fn remote_parent_handles_roots_and_relatives() {
        assert_eq!(parent_remote_path("/var/www"), "/var");
        assert_eq!(parent_remote_path("/var"), "/");
        assert_eq!(parent_remote_path("/"), "/");
        assert_eq!(parent_remote_path("project/src"), "project");
        assert_eq!(parent_remote_path("."), "..");
    }

    #[test]
    fn filters_entries_case_insensitively() {
        let entries = vec![
            Entry {
                name: "Report.txt".to_string(),
                is_dir: false,
                size: None,
                modified: None,
            },
            Entry {
                name: "src".to_string(),
                is_dir: true,
                size: None,
                modified: None,
            },
        ];

        let filtered = filter_entries(&entries, "rep");

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "Report.txt");
    }
}
