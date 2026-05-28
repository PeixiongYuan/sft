use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const RETENTION_SECS: i64 = 365 * 86_400;
const MAX_TIMESTAMPS: usize = 10_000;

#[derive(Debug, Clone, Default)]
pub struct HostStats {
    pub opens: u64,
    pub last_opened: Option<i64>,
    pub timestamps: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct HostHistory {
    path: PathBuf,
    entries: HashMap<String, HostStats>,
}

impl HostHistory {
    pub fn load_default() -> Self {
        let Some(path) = default_history_path() else {
            return Self {
                path: PathBuf::from("sft-history.tsv"),
                entries: HashMap::new(),
            };
        };
        Self::load(path).unwrap_or_else(|path| Self {
            path,
            entries: HashMap::new(),
        })
    }

    pub fn load(path: PathBuf) -> Result<Self, PathBuf> {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(_) => return Err(path),
        };

        let mut entries = HashMap::new();
        for line in content.lines() {
            let parts = line.splitn(4, '\t').collect::<Vec<_>>();
            let Some(alias) = parts.first().copied() else {
                continue;
            };
            let opens = parts
                .get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let last_opened = parts.get(2).and_then(|value| value.parse().ok());
            if !alias.is_empty() {
                let mut timestamps = parts
                    .get(3)
                    .map(|value| parse_timestamps(value))
                    .unwrap_or_default();
                if timestamps.is_empty()
                    && opens > 0
                    && let Some(last_opened) = last_opened
                {
                    timestamps.push(last_opened);
                }
                prune_timestamps(&mut timestamps, now_epoch());
                entries.insert(
                    alias.to_string(),
                    HostStats {
                        opens,
                        last_opened,
                        timestamps,
                    },
                );
            }
        }

        Ok(Self { path, entries })
    }

    pub fn stats(&self, alias: &str) -> HostStats {
        self.entries.get(alias).cloned().unwrap_or_default()
    }

    pub fn record_open(&mut self, alias: &str) {
        let now = now_epoch();
        let entry = self.entries.entry(alias.to_string()).or_default();
        entry.opens = entry.opens.saturating_add(1);
        entry.last_opened = Some(now);
        entry.timestamps.push(now);
        prune_timestamps(&mut entry.timestamps, now);
        let _ = self.save();
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut rows = self
            .entries
            .iter()
            .map(|(alias, stats)| {
                format!(
                    "{}\t{}\t{}\t{}",
                    alias,
                    stats.opens,
                    stats
                        .last_opened
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    stats
                        .timestamps
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect::<Vec<_>>();
        rows.sort();
        fs::write(&self.path, format!("{}\n", rows.join("\n")))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }

        Ok(())
    }
}

pub fn format_last_opened(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "never".to_string();
    };
    let diff = now_epoch().saturating_sub(ts);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 86400 * 30 {
        format!("{}d ago", diff / 86400)
    } else {
        format!("{}mo ago", diff / (86400 * 30))
    }
}

fn parse_timestamps(value: &str) -> Vec<i64> {
    value
        .split(',')
        .filter_map(|raw| raw.parse::<i64>().ok())
        .collect()
}

fn prune_timestamps(timestamps: &mut Vec<i64>, now: i64) {
    let cutoff = now.saturating_sub(RETENTION_SECS);
    timestamps.retain(|&ts| ts >= cutoff && ts <= now);
    timestamps.sort_unstable();
    if timestamps.len() > MAX_TIMESTAMPS {
        let excess = timestamps.len() - MAX_TIMESTAMPS;
        timestamps.drain(..excess);
    }
}

fn default_history_path() -> Option<PathBuf> {
    dirs::data_local_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .map(|dir| dir.join("sft").join("history.tsv"))
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_usage_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.tsv");
        let mut history = HostHistory::load(path.clone()).unwrap();
        history.record_open("web");
        history.record_open("web");

        let reloaded = HostHistory::load(path).unwrap();
        assert_eq!(reloaded.stats("web").opens, 2);
        assert!(reloaded.stats("web").last_opened.is_some());
        assert_eq!(reloaded.stats("web").timestamps.len(), 2);
    }

    #[test]
    fn loads_legacy_rows_with_last_opened_as_single_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.tsv");
        let ts = now_epoch() - 60;
        fs::write(&path, format!("web\t3\t{ts}\n")).unwrap();

        let history = HostHistory::load(path).unwrap();
        let stats = history.stats("web");

        assert_eq!(stats.opens, 3);
        assert_eq!(stats.last_opened, Some(ts));
        assert_eq!(stats.timestamps, vec![ts]);
    }

    #[test]
    fn loads_timestamp_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.tsv");
        let now = now_epoch();
        let t1 = now - 3;
        let t2 = now - 2;
        let t3 = now - 1;
        fs::write(&path, format!("web\t3\t{t3}\t{t1},{t2},{t3}\n")).unwrap();

        let history = HostHistory::load(path).unwrap();
        let stats = history.stats("web");

        assert_eq!(stats.opens, 3);
        assert_eq!(stats.timestamps, vec![t1, t2, t3]);
    }
}
