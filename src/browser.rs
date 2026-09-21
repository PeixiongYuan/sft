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
