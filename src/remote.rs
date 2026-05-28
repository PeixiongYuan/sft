use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::askpass::PasswordHandle;
use crate::browser::{self, Entry, Pane, SortMode};

const CONTROL_PERSIST_SECS: u16 = 3600;
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const TCP_OVERALL_TIMEOUT: Duration = Duration::from_secs(5);

pub const DEFAULT_SSH_PORT: u16 = 22;
pub const SLOW_REACHABILITY_MS: u32 = 500;

#[derive(Debug, Clone)]
pub struct Connection {
    pub host: String,
    pub config_path: Option<PathBuf>,
    pub password: Option<PasswordHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpCheckTarget {
    pub hostname: String,
    pub port: u16,
    pub via_proxy_jump: Option<String>,
}

pub fn remote_pwd(connection: &Connection) -> Result<String> {
    let output = run_sftp_batch(connection, "pwd\n")
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("failed to run sftp for {}", connection.host))?;
    parse_sftp_pwd(&output).context("sftp pwd returned no directory")
}

pub fn fetch_remote_listing(
    connection: &Connection,
    path: &str,
    show_hidden: bool,
    sort: SortMode,
) -> Result<Vec<Entry>, String> {
    let script = format!("cd {}\nls -lah\n", sftp_quote(path));
    let output = run_sftp_batch(connection, &script)?;
    Ok(browser::parse_ls_output(&output, show_hidden, sort))
}

pub fn fetch_remote_directories(
    connection: &Connection,
    path: &str,
) -> Result<Vec<String>, String> {
    let mut dirs = fetch_remote_listing(connection, path, true, SortMode::Name)?
        .into_iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    dirs.sort_by_key(|name| name.to_ascii_lowercase());
    dirs.dedup();
    Ok(dirs)
}

pub fn run_sftp_transfer(
    connection: &Connection,
    source: Pane,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
    has_dirs: bool,
) -> Result<(), String> {
    let script = build_sftp_transfer_batch(source, local_path, remote_path, names, has_dirs);
    run_sftp_batch(connection, &script).map(|_| ())
}

pub fn create_remote_dir(connection: &Connection, path: &str, name: &str) -> Result<(), String> {
    let target = browser::join_remote_path(path, name);
    run_sftp_batch(connection, &format!("mkdir {}\n", sftp_quote(&target))).map(|_| ())
}

pub fn rename_remote_entry(
    connection: &Connection,
    path: &str,
    old_name: &str,
    new_name: &str,
) -> Result<(), String> {
    let source = browser::join_remote_path(path, old_name);
    let target = browser::join_remote_path(path, new_name);
    run_sftp_batch(
        connection,
        &format!("rename {} {}\n", sftp_quote(&source), sftp_quote(&target)),
    )
    .map(|_| ())
}

pub fn delete_remote_entries(
    connection: &Connection,
    path: &str,
    names: &[String],
    has_dirs: bool,
) -> Result<(), String> {
    if names.is_empty() {
        return Ok(());
    }
    if !has_dirs {
        let script = names
            .iter()
            .map(|name| {
                let target = browser::join_remote_path(path, name);
                format!("rm {}\n", sftp_quote(&target))
            })
            .collect::<String>();
        return run_sftp_batch(connection, &script).map(|_| ());
    }

    let targets = names
        .iter()
        .map(|name| browser::shell_escape(&browser::join_remote_path(path, name)))
        .collect::<Vec<_>>()
        .join(" ");
    run_remote_shell(connection, &format!("rm -rf -- {targets}"))
}

pub fn remote_entries_size(
    connection: &Connection,
    path: &str,
    names: &[String],
) -> Result<u64, String> {
    if names.is_empty() {
        return Ok(0);
    }

    let targets = names
        .iter()
        .map(|name| browser::shell_escape(&browser::join_remote_path(path, name)))
        .collect::<Vec<_>>()
        .join(" ");
    let command = format!(
        r#"sft_size() {{
    if [ -f "$1" ]; then
        stat -c %s -- "$1" 2>/dev/null || stat -f %z "$1" 2>/dev/null || wc -c < "$1" 2>/dev/null || printf 0
    elif [ -d "$1" ]; then
        find "$1" -type f -exec sh -c 'for f do stat -c %s -- "$f" 2>/dev/null || stat -f %z "$f" 2>/dev/null || wc -c < "$f" 2>/dev/null || printf 0; done' sh {{}} + 2>/dev/null | awk '{{s += $1}} END {{print s+0}}'
    else
        printf 0
    fi
}}
total=0
for p in {targets}; do
    size=$(sft_size "$p")
    total=$((total + size))
done
printf '%s\n' "$total"
"#
    );
    let output = run_remote_shell_output(connection, &command)?;
    parse_remote_size_output(&output)
}

pub fn check_reachability(host: &str, config_path: Option<&Path>) -> Option<u32> {
    let target = resolve_tcp_check_target(host, config_path);
    tcp_connect_rtt(&target.hostname, target.port)
}

pub fn resolve_tcp_check_target(host: &str, config_path: Option<&Path>) -> TcpCheckTarget {
    let direct = resolve_effective_ssh(host, config_path).unwrap_or_else(|| EffectiveSsh {
        hostname: host.to_string(),
        port: DEFAULT_SSH_PORT,
        proxy_jump: None,
    });

    let Some(jump) = direct.proxy_jump.as_deref().and_then(first_proxy_jump) else {
        return TcpCheckTarget {
            hostname: direct.hostname,
            port: direct.port,
            via_proxy_jump: None,
        };
    };

    let jump = parse_proxy_jump_target(jump);
    let resolved_jump = resolve_effective_ssh(&jump.host, config_path);
    let hostname = resolved_jump
        .as_ref()
        .map(|resolved| resolved.hostname.clone())
        .unwrap_or_else(|| jump.host.clone());
    let port = jump
        .port
        .or_else(|| resolved_jump.as_ref().map(|resolved| resolved.port))
        .unwrap_or(DEFAULT_SSH_PORT);

    TcpCheckTarget {
        hostname,
        port,
        via_proxy_jump: Some(jump.host),
    }
}

fn build_sftp_transfer_batch(
    source: Pane,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
    has_dirs: bool,
) -> String {
    let recursive = if has_dirs { "R" } else { "" };
    let flags = format!("-p{recursive}");
    let mut script = String::new();

    match source {
        Pane::Local => {
            let remote_dir = browser::ensure_remote_dir(remote_path);
            for name in names {
                let local = local_path.join(name).to_string_lossy().into_owned();
                script.push_str(&format!(
                    "put {flags} {} {}\n",
                    sftp_quote(&local),
                    sftp_quote(&remote_dir)
                ));
            }
        }
        Pane::Remote => {
            let local_dir = local_path.to_string_lossy().into_owned();
            for name in names {
                let remote = browser::join_remote_path(remote_path, name);
                script.push_str(&format!(
                    "get {flags} {} {}\n",
                    sftp_quote(&remote),
                    sftp_quote(&local_dir)
                ));
            }
        }
    }

    script
}

fn run_sftp_batch(connection: &Connection, script: &str) -> Result<String, String> {
    let mut command = sftp_command(connection);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn sftp: {err}"))?;

    let Some(mut stdin) = child.stdin.take() else {
        return Err("failed to open sftp stdin".to_string());
    };
    stdin
        .write_all(script.as_bytes())
        .map_err(|err| format!("failed to write sftp batch: {err}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait for sftp: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(useful_error(
            &format!("{stderr}\n{stdout}"),
            output.status.code(),
        ))
    }
}

fn sftp_command(connection: &Connection) -> Command {
    let mut command = Command::new("sftp");
    append_config(&mut command, connection);
    command
        .arg("-q")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ClearAllForwardings=yes");
    append_mux_options(&mut command);
    append_auth_options(&mut command, connection);
    command.arg("-b").arg("-").arg(&connection.host);
    command
}

fn run_remote_shell(connection: &Connection, remote_command: &str) -> Result<(), String> {
    let output = ssh_command(connection, remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("failed to spawn ssh: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(useful_error(&stderr, output.status.code()))
    }
}

fn run_remote_shell_output(
    connection: &Connection,
    remote_command: &str,
) -> Result<String, String> {
    let output = ssh_command(connection, remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("failed to spawn ssh: {err}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(useful_error(
            &format!("{stderr}\n{stdout}"),
            output.status.code(),
        ))
    }
}

fn parse_remote_size_output(output: &str) -> Result<u64, String> {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| "remote size command returned no output".to_string())?
        .parse::<u64>()
        .map_err(|err| format!("could not parse remote size: {err}"))
}

fn parse_sftp_pwd(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix("Remote working directory: ")
                .or_else(|| line.strip_prefix("Remote working directory:"))
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(ToString::to_string)
        })
        .or_else(|| {
            let trimmed = output.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
}

fn sftp_quote(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn ssh_command(connection: &Connection, remote_command: &str) -> Command {
    let mut command = Command::new("ssh");
    append_config(&mut command, connection);
    command
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ClearAllForwardings=yes");
    append_mux_options(&mut command);
    append_auth_options(&mut command, connection);
    command.arg("--").arg(&connection.host).arg(remote_command);
    command
}

fn append_config(command: &mut Command, connection: &Connection) {
    if let Some(config_path) = &connection.config_path {
        command.arg("-F").arg(config_path);
    }
}

fn append_mux_options(command: &mut Command) {
    if let Some(control_path) = control_path() {
        command
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg(format!("ControlPersist={CONTROL_PERSIST_SECS}"))
            .arg("-o")
            .arg(format!("ControlPath={control_path}"));
    }
}

fn append_password(command: &mut Command, connection: &Connection) {
    if let Some(password) = &connection.password {
        password.configure_command(command);
    }
}

fn append_auth_options(command: &mut Command, connection: &Connection) {
    if connection.password.is_some() {
        detach_controlling_terminal(command);
        command
            .arg("-o")
            .arg("BatchMode=no")
            .arg("-o")
            .arg("NumberOfPasswordPrompts=1")
            .arg("-o")
            .arg("PreferredAuthentications=password,keyboard-interactive,publickey");
    } else {
        command
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("PasswordAuthentication=no")
            .arg("-o")
            .arg("KbdInteractiveAuthentication=no")
            .arg("-o")
            .arg("NumberOfPasswordPrompts=0");
    }
    append_password(command, connection);
}

#[cfg(unix)]
fn detach_controlling_terminal(command: &mut Command) {
    use std::os::raw::c_int;
    use std::os::unix::process::CommandExt;

    unsafe extern "C" {
        fn setsid() -> c_int;
    }

    unsafe {
        command.pre_exec(|| {
            if setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn detach_controlling_terminal(_command: &mut Command) {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EffectiveSsh {
    hostname: String,
    port: u16,
    proxy_jump: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProxyJumpTarget {
    host: String,
    port: Option<u16>,
}

fn resolve_effective_ssh(host: &str, config_path: Option<&Path>) -> Option<EffectiveSsh> {
    let mut command = Command::new("ssh");
    command.arg("-G");
    if let Some(config_path) = config_path {
        command.arg("-F").arg(config_path);
    }
    command.arg("--").arg(host);

    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_effective_ssh(&String::from_utf8_lossy(&output.stdout), host)
}

fn parse_effective_ssh(output: &str, fallback_host: &str) -> Option<EffectiveSsh> {
    let mut hostname = None;
    let mut port = None;
    let mut proxy_jump = None;

    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let value = parts.collect::<Vec<_>>().join(" ");
        if key.eq_ignore_ascii_case("hostname") {
            hostname = Some(value);
        } else if key.eq_ignore_ascii_case("port") {
            port = value.parse::<u16>().ok();
        } else if key.eq_ignore_ascii_case("proxyjump")
            && !value.is_empty()
            && !value.eq_ignore_ascii_case("none")
            && value != "-"
        {
            proxy_jump = Some(value);
        }
    }

    Some(EffectiveSsh {
        hostname: hostname.unwrap_or_else(|| fallback_host.to_string()),
        port: port.unwrap_or(DEFAULT_SSH_PORT),
        proxy_jump,
    })
}

fn first_proxy_jump(value: &str) -> Option<&str> {
    value
        .split(',')
        .map(str::trim)
        .find(|hop| !hop.is_empty() && *hop != "-")
}

fn parse_proxy_jump_target(value: &str) -> ProxyJumpTarget {
    let value = value.trim();
    let without_user = value.rsplit_once('@').map_or(value, |(_, host)| host);

    if let Some(rest) = without_user.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail.strip_prefix(':').and_then(|raw| raw.parse().ok());
        return ProxyJumpTarget {
            host: host.to_string(),
            port,
        };
    }

    let colon_count = without_user.matches(':').count();
    if colon_count == 1
        && let Some((host, raw_port)) = without_user.rsplit_once(':')
        && let Ok(port) = raw_port.parse::<u16>()
    {
        return ProxyJumpTarget {
            host: host.to_string(),
            port: Some(port),
        };
    }

    ProxyJumpTarget {
        host: without_user.to_string(),
        port: None,
    }
}

fn tcp_connect_rtt(hostname: &str, port: u16) -> Option<u32> {
    let hostname = hostname.trim();
    if hostname.is_empty() {
        return None;
    }

    let clean = hostname.trim_start_matches('[').trim_end_matches(']');
    let addr = if clean.contains(':') {
        format!("[{clean}]:{port}")
    } else {
        format!("{clean}:{port}")
    };

    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let start = Instant::now();
        let connected = addr.to_socket_addrs().is_ok_and(|addrs| {
            addrs
                .into_iter()
                .any(|addr| TcpStream::connect_timeout(&addr, TCP_CONNECT_TIMEOUT).is_ok())
        });
        let rtt_ms = connected.then(|| start.elapsed().as_millis().min(u32::MAX as u128) as u32);
        let _ = done_tx.send(rtt_ms);
    });

    done_rx.recv_timeout(TCP_OVERALL_TIMEOUT).unwrap_or(None)
}

fn control_path() -> Option<String> {
    let base = dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))?
        .join("sft")
        .join("ssh_mux");
    if std::fs::create_dir_all(&base).is_err() {
        return None;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }

    Some(base.join("%C").to_string_lossy().into_owned())
}

fn useful_error(stderr: &str, code: Option<i32>) -> String {
    let filtered = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("Warning:"))
        .filter(|line| !line.starts_with("Connection to "))
        .collect::<Vec<_>>()
        .join("\n");

    if filtered.is_empty() {
        format!("command failed with exit code {}", code.unwrap_or(1))
    } else {
        filtered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_effective_ssh_output() {
        let parsed = parse_effective_ssh(
            "user deploy\nhostname 10.0.0.4\nport 2222\nproxyjump bastion\n",
            "web",
        )
        .unwrap();

        assert_eq!(parsed.hostname, "10.0.0.4");
        assert_eq!(parsed.port, 2222);
        assert_eq!(parsed.proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn parses_proxy_jump_targets() {
        let with_user = parse_proxy_jump_target("deploy@jump.example.com:2200");
        assert_eq!(with_user.host, "jump.example.com");
        assert_eq!(with_user.port, Some(2200));

        let ipv6 = parse_proxy_jump_target("root@[2001:db8::1]:2222");
        assert_eq!(ipv6.host, "2001:db8::1");
        assert_eq!(ipv6.port, Some(2222));

        let bare_ipv6 = parse_proxy_jump_target("2001:db8::2");
        assert_eq!(bare_ipv6.host, "2001:db8::2");
        assert_eq!(bare_ipv6.port, None);
    }

    #[test]
    fn builds_sftp_upload_batch() {
        let batch = build_sftp_transfer_batch(
            Pane::Local,
            Path::new("/tmp/local"),
            "/var/www",
            &[String::from("app bundle.tar")],
            false,
        );

        assert_eq!(
            batch,
            "put -p \"/tmp/local/app bundle.tar\" \"/var/www/\"\n"
        );
    }

    #[test]
    fn builds_sftp_recursive_download_batch() {
        let batch = build_sftp_transfer_batch(
            Pane::Remote,
            Path::new("/tmp/local"),
            "/var/www",
            &[String::from("assets")],
            true,
        );

        assert_eq!(batch, "get -pR \"/var/www/assets\" \"/tmp/local\"\n");
    }

    #[test]
    fn quotes_sftp_paths() {
        assert_eq!(
            sftp_quote(r#"/tmp/a "quote" dir"#),
            r#""/tmp/a \"quote\" dir""#
        );
    }

    #[test]
    fn parses_sftp_pwd_output() {
        assert_eq!(
            parse_sftp_pwd("Remote working directory: /home/deploy\n").as_deref(),
            Some("/home/deploy")
        );
    }

    #[test]
    fn parses_remote_size_output_from_last_line() {
        assert_eq!(parse_remote_size_output("debug\n42\n").unwrap(), 42);
    }
}
