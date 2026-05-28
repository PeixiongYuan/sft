use std::cmp::Reverse;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::browser::Pane;

const MAX_RECENTS_PER_PANE: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    pub host: String,
    pub pane: Pane,
    pub path: String,
    pub pinned: bool,
    pub last_used: i64,
}

#[derive(Debug, Clone)]
pub struct PathStore {
    path: PathBuf,
    entries: Vec<PathEntry>,
}

impl PathStore {
    pub fn load_default() -> Self {
        let Some(path) = default_path() else {
            return Self {
                path: PathBuf::from("sft-paths.tsv"),
                entries: Vec::new(),
            };
        };
        Self::load(path).unwrap_or_else(|path| Self {
            path,
            entries: Vec::new(),
        })
    }

    pub fn load(path: PathBuf) -> Result<Self, PathBuf> {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(_) => return Err(path),
        };

        let entries = content.lines().filter_map(parse_entry).collect::<Vec<_>>();
        Ok(Self { path, entries })
    }

    pub fn entries(&self, host: &str, pane: Pane) -> Vec<PathEntry> {
        let mut entries = self
            .entries
            .iter()
            .filter(|entry| entry.host == host && entry.pane == pane)
            .cloned()
            .collect::<Vec<_>>();
        sort_entries(&mut entries);
        entries
    }

    pub fn record_recent(&mut self, host: &str, pane: Pane, path: &str) {
        self.upsert(host, pane, path, None);
    }

    pub fn toggle_bookmark(&mut self, host: &str, pane: Pane, path: &str) -> bool {
        let pinned = self
            .entries
            .iter()
            .find(|entry| entry.host == host && entry.pane == pane && entry.path == path)
            .map(|entry| !entry.pinned)
            .unwrap_or(true);
        self.upsert(host, pane, path, Some(pinned));
        pinned
    }

    fn upsert(&mut self, host: &str, pane: Pane, path: &str, pinned: Option<bool>) {
        let now = now_epoch();
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.host == host && entry.pane == pane && entry.path == path)
        {
            entry.last_used = now;
            if let Some(pinned) = pinned {
                entry.pinned = pinned;
            }
        } else {
            self.entries.push(PathEntry {
                host: host.to_string(),
                pane,
                path: path.to_string(),
                pinned: pinned.unwrap_or(false),
                last_used: now,
            });
        }
        self.prune_recents(host, pane);
        let _ = self.save();
    }

    fn prune_recents(&mut self, host: &str, pane: Pane) {
        let mut recents = self
            .entries
            .iter()
            .filter(|entry| entry.host == host && entry.pane == pane && !entry.pinned)
            .map(|entry| (entry.path.clone(), entry.last_used))
            .collect::<Vec<_>>();
        recents.sort_by_key(|entry| Reverse(entry.1));
        let keep = recents
            .iter()
            .take(MAX_RECENTS_PER_PANE)
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();

        self.entries.retain(|entry| {
            entry.host != host
                || entry.pane != pane
                || entry.pinned
                || keep.iter().any(|path| path == &entry.path)
        });
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut rows = self
            .entries
            .iter()
            .map(|entry| {
                format!(
                    "{}\t{}\t{}\t{}\t{}",
                    clean_field(&entry.host),
                    pane_field(entry.pane),
                    u8::from(entry.pinned),
                    entry.last_used,
                    clean_field(&entry.path)
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

fn parse_entry(line: &str) -> Option<PathEntry> {
    let parts = line.splitn(5, '\t').collect::<Vec<_>>();
    let host = parts.first()?.to_string();
    let pane = parse_pane(parts.get(1).copied()?)?;
    let pinned = parts.get(2).is_some_and(|value| *value == "1");
    let last_used = parts.get(3)?.parse().ok()?;
    let path = parts.get(4)?.to_string();
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(PathEntry {
        host,
        pane,
        path,
        pinned,
        last_used,
    })
}

fn sort_entries(entries: &mut [PathEntry]) {
    entries.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| b.last_used.cmp(&a.last_used))
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn parse_pane(value: &str) -> Option<Pane> {
    match value {
        "local" => Some(Pane::Local),
        "remote" => Some(Pane::Remote),
        _ => None,
    }
}

fn pane_field(pane: Pane) -> &'static str {
    match pane {
        Pane::Local => "local",
        Pane::Remote => "remote",
    }
}

fn clean_field(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

fn default_path() -> Option<PathBuf> {
    dirs::data_local_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .map(|dir| dir.join("sft").join("paths.tsv"))
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
    fn records_and_toggles_bookmarks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paths.tsv");
        let mut store = PathStore::load(path.clone()).unwrap();
        store.record_recent("web", Pane::Remote, "/var/log");
        assert!(!store.entries("web", Pane::Remote)[0].pinned);

        assert!(store.toggle_bookmark("web", Pane::Remote, "/var/log"));
        let reloaded = PathStore::load(path).unwrap();
        assert!(reloaded.entries("web", Pane::Remote)[0].pinned);
    }
}
