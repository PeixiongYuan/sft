use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostDraft {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: String,
    pub identity_file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    pub alias: String,
    pub patterns: Vec<String>,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<String>,
    pub identity_file: Option<String>,
    pub start: usize,
    pub end: usize,
}

impl HostEntry {
    pub fn editable(&self) -> bool {
        self.patterns.len() == 1 && self.patterns[0] == self.alias
    }

    pub fn to_draft(&self) -> HostDraft {
        HostDraft {
            alias: self.alias.clone(),
            hostname: self.hostname.clone().unwrap_or_default(),
            user: self.user.clone().unwrap_or_default(),
            port: self.port.clone().unwrap_or_default(),
            identity_file: self.identity_file.clone().unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SshConfig {
    pub path: PathBuf,
    lines: Vec<String>,
    hosts: Vec<HostEntry>,
}

impl SshConfig {
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        };
        Ok(Self::parse(path, &content))
    }

    pub fn parse(path: PathBuf, content: &str) -> Self {
        let mut lines: Vec<String> = content.lines().map(ToString::to_string).collect();
        if content.ends_with('\n') {
            while lines.last().is_some_and(|line| line.is_empty()) {
                lines.pop();
            }
        }
        let hosts = parse_hosts(&lines);
        Self { path, lines, hosts }
    }

    pub fn hosts(&self) -> &[HostEntry] {
        &self.hosts
    }

    pub fn reload(&mut self) -> io::Result<()> {
        *self = Self::load(self.path.clone())?;
        Ok(())
    }

    pub fn add_host(&mut self, draft: &HostDraft) -> Result<(), String> {
        validate_draft(draft, false)?;
        if self.hosts.iter().any(|host| host.alias == draft.alias) {
            return Err(format!("Host '{}' already exists.", draft.alias));
        }

        if !self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.lines.extend(render_new_block(draft));
        self.save_and_reparse().map_err(|err| err.to_string())
    }

    pub fn update_host(&mut self, original_alias: &str, draft: &HostDraft) -> Result<(), String> {
        validate_draft(draft, true)?;
        if draft.alias != original_alias && self.hosts.iter().any(|host| host.alias == draft.alias)
        {
            return Err(format!("Host '{}' already exists.", draft.alias));
        }

        let entry = self
            .hosts
            .iter()
            .find(|host| host.alias == original_alias)
            .cloned()
            .ok_or_else(|| format!("Host '{original_alias}' not found."))?;
        if !entry.editable() {
            return Err("Only single-alias Host blocks can be edited safely.".to_string());
        }

        self.lines[entry.start] = format!("Host {}", draft.alias);
        self.hosts = parse_hosts(&self.lines);
        self.set_directive(&draft.alias, "HostName", &draft.hostname);
        self.set_directive(&draft.alias, "User", &draft.user);
        self.set_directive(&draft.alias, "Port", &draft.port);
        self.set_directive(&draft.alias, "IdentityFile", &draft.identity_file);
        self.save_and_reparse().map_err(|err| err.to_string())
    }

    pub fn delete_host(&mut self, alias: &str) -> Result<(), String> {
        let entry = self
            .hosts
            .iter()
            .find(|host| host.alias == alias)
            .cloned()
            .ok_or_else(|| format!("Host '{alias}' not found."))?;
        if !entry.editable() {
            return Err("Only single-alias Host blocks can be deleted safely.".to_string());
        }

        let mut start = entry.start;
        let mut end = entry.end;
        if start > 0 && self.lines[start - 1].trim().is_empty() {
            start -= 1;
        } else if end < self.lines.len() && self.lines[end].trim().is_empty() {
            end += 1;
        }
        self.lines.drain(start..end);
        self.save_and_reparse().map_err(|err| err.to_string())
    }

    fn set_directive(&mut self, alias: &str, key: &str, value: &str) {
        let Some(entry) = self.hosts.iter().find(|host| host.alias == alias).cloned() else {
            return;
        };
        let value = value.trim();
        let existing = (entry.start + 1..entry.end).find(|&idx| {
            directive_key(&self.lines[idx])
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(key))
        });

        match (existing, value.is_empty()) {
            (Some(idx), true) => {
                self.lines.remove(idx);
                self.hosts = parse_hosts(&self.lines);
            }
            (Some(idx), false) => {
                let indent = leading_whitespace(&self.lines[idx]);
                self.lines[idx] = format!("{indent}{key} {value}");
            }
            (None, false) => {
                self.lines.insert(entry.end, format!("  {key} {value}"));
                self.hosts = parse_hosts(&self.lines);
            }
            (None, true) => {}
        }
    }

    fn save_and_reparse(&mut self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = if self.lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", self.lines.join("\n"))
        };
        fs::write(&self.path, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }
        self.reload()
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh/config"))
}

fn parse_hosts(lines: &[String]) -> Vec<HostEntry> {
    let mut blocks = Vec::new();
    let mut current: Option<(usize, Vec<String>)> = None;

    for (idx, line) in lines.iter().enumerate() {
        let Some(key) = directive_key(line) else {
            continue;
        };
        if key.eq_ignore_ascii_case("Host") || key.eq_ignore_ascii_case("Match") {
            if let Some((start, patterns)) = current.take() {
                blocks.extend(entries_for_block(lines, start, idx, patterns));
            }
            if key.eq_ignore_ascii_case("Host") {
                let patterns = directive_value(line)
                    .map(|value| value.split_whitespace().map(ToString::to_string).collect())
                    .unwrap_or_default();
                current = Some((idx, patterns));
            }
        }
    }

    if let Some((start, patterns)) = current {
        blocks.extend(entries_for_block(lines, start, lines.len(), patterns));
    }
    blocks
}

fn entries_for_block(
    lines: &[String],
    start: usize,
    end: usize,
    patterns: Vec<String>,
) -> Vec<HostEntry> {
    let aliases = patterns
        .iter()
        .filter(|pattern| is_connectable_pattern(pattern))
        .cloned()
        .collect::<Vec<_>>();
    if aliases.is_empty() {
        return Vec::new();
    }

    let mut hostname = None;
    let mut user = None;
    let mut port = None;
    let mut identity_file = None;
    for line in &lines[start + 1..end] {
        let Some(key) = directive_key(line) else {
            continue;
        };
        let value = directive_value(line).unwrap_or("").trim().to_string();
        if key.eq_ignore_ascii_case("HostName") {
            hostname = Some(value);
        } else if key.eq_ignore_ascii_case("User") {
            user = Some(value);
        } else if key.eq_ignore_ascii_case("Port") {
            port = Some(value);
        } else if key.eq_ignore_ascii_case("IdentityFile") {
            identity_file = Some(value);
        }
    }

    aliases
        .into_iter()
        .map(|alias| HostEntry {
            alias,
            patterns: patterns.clone(),
            hostname: hostname.clone(),
            user: user.clone(),
            port: port.clone(),
            identity_file: identity_file.clone(),
            start,
            end,
        })
        .collect()
}

fn render_new_block(draft: &HostDraft) -> Vec<String> {
    let mut lines = vec![format!("Host {}", draft.alias.trim())];
    for (key, value) in [
        ("HostName", draft.hostname.trim()),
        ("User", draft.user.trim()),
        ("Port", draft.port.trim()),
        ("IdentityFile", draft.identity_file.trim()),
    ] {
        if !value.is_empty() {
            lines.push(format!("  {key} {value}"));
        }
    }
    lines
}

fn validate_draft(draft: &HostDraft, hostname_optional: bool) -> Result<(), String> {
    let alias = draft.alias.trim();
    if alias.is_empty() {
        return Err("Host alias is required.".to_string());
    }
    if alias.contains(char::is_whitespace) {
        return Err("Host alias cannot contain whitespace.".to_string());
    }
    if !is_connectable_pattern(alias) {
        return Err("Host alias cannot contain wildcard or negation characters.".to_string());
    }
    if !hostname_optional && draft.hostname.trim().is_empty() {
        return Err("HostName is required for new hosts.".to_string());
    }
    if !draft.port.trim().is_empty() && draft.port.trim().parse::<u16>().is_err() {
        return Err("Port must be a number from 1 to 65535.".to_string());
    }
    Ok(())
}

fn is_connectable_pattern(pattern: &str) -> bool {
    !pattern.starts_with('!')
        && !pattern.contains('*')
        && !pattern.contains('?')
        && !pattern.contains('[')
        && !pattern.contains(']')
}

fn directive_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    trimmed.split_whitespace().next()
}

fn directive_value(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let key = trimmed.split_whitespace().next()?;
    Some(trimmed[key.len()..].trim_start())
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

#[allow(dead_code)]
fn _assert_path(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connectable_hosts_and_skips_wildcards() {
        let cfg = SshConfig::parse(
            PathBuf::from("config"),
            r#"
Host *
  ServerAliveInterval 30

Host web
  HostName 10.0.0.2
  User deploy
  Port 2222

Host *.internal
  User root

Match user root
  HostName ignored
"#,
        );

        assert_eq!(cfg.hosts().len(), 1);
        assert_eq!(cfg.hosts()[0].alias, "web");
        assert_eq!(cfg.hosts()[0].hostname.as_deref(), Some("10.0.0.2"));
        assert_eq!(cfg.hosts()[0].user.as_deref(), Some("deploy"));
        assert_eq!(cfg.hosts()[0].port.as_deref(), Some("2222"));
    }

    #[test]
    fn preserves_unknown_directives_when_editing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        fs::write(
            &path,
            "Host web\n  HostName old\n  ProxyJump jump\n  User root\n",
        )
        .unwrap();
        let mut cfg = SshConfig::load(path.clone()).unwrap();

        cfg.update_host(
            "web",
            &HostDraft {
                alias: "app".to_string(),
                hostname: "new".to_string(),
                user: "deploy".to_string(),
                port: "2222".to_string(),
                identity_file: String::new(),
            },
        )
        .unwrap();

        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("Host app\n"));
        assert!(content.contains("  HostName new\n"));
        assert!(content.contains("  ProxyJump jump\n"));
        assert!(content.contains("  User deploy\n"));
        assert!(content.contains("  Port 2222\n"));
    }
}
