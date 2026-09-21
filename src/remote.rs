use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::askpass::PasswordHandle;
use crate::browser::{self, Entry, SortMode};
use crate::session;

const CONTROL_PERSIST_SECS: u32 = 14_400;
const SSH_OPERATION_CONNECT_TIMEOUT_SECS: u32 = 60;
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const TCP_OVERALL_TIMEOUT: Duration = Duration::from_secs(5);

pub const DEFAULT_SSH_PORT: u16 = 22;
pub const SLOW_REACHABILITY_MS: u32 = 500;

/// How long to wait for a host to answer the SFTP subsystem handshake.
///
/// This is a backstop for a channel that never comes up, not a latency
/// budget: ssh already bounds connecting and keeps the link alive, while the
/// server can spend a long time on per-session setup (PAM, directory
/// services, mounting a network home) before the subsystem replies. Busy
/// login nodes routinely take tens of seconds, so keep this well clear of
/// what a healthy-but-slow host needs.
pub const SFTP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// Says how long was waited, so a timeout reads as "this host is unusually
/// slow" rather than "sft gave up for no stated reason".
pub fn handshake_timeout_message() -> String {
    format!(
        "host did not open an sftp session within {}s",
        SFTP_HANDSHAKE_TIMEOUT.as_secs()
    )
}

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

#[derive(Debug, Clone)]
pub struct InitialRemoteListing {
    pub home: String,
    pub path: String,
    pub entries: Vec<Entry>,
}

pub fn fetch_initial_remote_listing(
    connection: &Connection,
    path: Option<&str>,
    show_hidden: bool,
    sort: SortMode,
) -> Result<InitialRemoteListing, String> {
    let home = session::realpath(connection, ".")?;
    let path = match path {
        Some(requested) => session::realpath(connection, requested)?,
        None => home.clone(),
    };
    let entries = session::list_entries(connection, &path, show_hidden, sort)?;

    Ok(InitialRemoteListing {
        home,
        path,
        entries,
    })
}

pub fn fetch_remote_listing(
    connection: &Connection,
    path: &str,
    show_hidden: bool,
    sort: SortMode,
) -> Result<Vec<Entry>, String> {
    session::list_entries(connection, path, show_hidden, sort)
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

pub fn create_remote_dir(connection: &Connection, path: &str, name: &str) -> Result<(), String> {
    session::create_dir(connection, &browser::join_remote_path(path, name))
}

pub fn rename_remote_entry(
    connection: &Connection,
    path: &str,
    old_name: &str,
    new_name: &str,
) -> Result<(), String> {
    let source = browser::join_remote_path(path, old_name);
    let target = browser::join_remote_path(path, new_name);
    session::rename(connection, &source, &target)
}

pub fn delete_remote_entries(
    connection: &Connection,
    path: &str,
    names: &[String],
) -> Result<(), String> {
    if names.is_empty() {
        return Ok(());
    }
    let targets = names
        .iter()
        .map(|name| browser::join_remote_path(path, name))
        .collect::<Vec<_>>();
    session::remove(connection, targets)
}

/// Close this host's control session, e.g. when leaving its file browser.
pub fn close_session(connection: &Connection) {
    session::shutdown(connection);
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

/// Build an `ssh` command that opens the raw SFTP subsystem channel on the
/// (reused) connection, with stdin/stdout speaking the SFTP wire protocol.
pub fn ssh_sftp_subsystem_command(connection: &Connection) -> Command {
    let mut command = Command::new("ssh");
    append_config(&mut command, connection);
    append_proxy_jump_mux_options(&mut command, connection);
    command
        .arg("-o")
        .arg(format!(
            "ConnectTimeout={SSH_OPERATION_CONNECT_TIMEOUT_SECS}"
        ))
        .arg("-o")
        .arg("ClearAllForwardings=yes");
    append_mux_options(&mut command, connection);
    append_auth_options(&mut command, connection);
    command.arg("-s").arg("--").arg(&connection.host).arg("sftp");
    command
}

fn append_config(command: &mut Command, connection: &Connection) {
    if let Some(config_path) = &connection.config_path {
        command.arg("-F").arg(config_path);
    }
}

fn append_proxy_jump_mux_options(command: &mut Command, connection: &Connection) {
    let Some(proxy_command) = proxy_jump_mux_command(connection) else {
        return;
    };
    command
        .arg("-o")
        .arg(format!("ProxyCommand={proxy_command}"));
}

fn append_mux_options(command: &mut Command, connection: &Connection) {
    if let Some(control_path) = connection_control_path(connection) {
        command
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg(format!("ControlPersist={CONTROL_PERSIST_SECS}"))
            .arg("-o")
            .arg(format!("ControlPath={control_path}"));
    }
}

pub fn ensure_connection_master(connection: &Connection) -> Result<(), String> {
    ensure_proxy_jump_master(connection)?;

    let Some(control_path) = connection_control_path(connection) else {
        return Ok(());
    };
    if connection_master_is_alive(connection, &control_path) {
        consume_password(connection);
        return Ok(());
    }
    let _ = std::fs::remove_file(&control_path);
    let mut command = Command::new("ssh");
    append_config(&mut command, connection);
    append_proxy_jump_mux_options(&mut command, connection);
    command
        .arg("-M")
        .arg("-N")
        .arg("-f")
        .arg("-o")
        .arg(format!(
            "ConnectTimeout={SSH_OPERATION_CONNECT_TIMEOUT_SECS}"
        ))
        .arg("-o")
        .arg("ClearAllForwardings=yes")
        .arg("-o")
        .arg("ControlMaster=yes")
        .arg("-o")
        .arg(format!("ControlPersist={CONTROL_PERSIST_SECS}"))
        .arg("-o")
        .arg(format!("ControlPath={control_path}"))
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg("-o")
        .arg("ServerAliveCountMax=3");
    append_auth_options(&mut command, connection);
    command.arg("--").arg(&connection.host);

    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("failed to spawn ssh master: {err}"))?;
    if output.status.success() && connection_master_is_alive(connection, &control_path) {
        consume_password(connection);
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(useful_error(&stderr, output.status.code()))
    }
}

fn connection_master_is_alive(connection: &Connection, control_path: &str) -> bool {
    let mut command = Command::new("ssh");
    append_config(&mut command, connection);
    command
        .arg("-S")
        .arg(control_path)
        .arg("-O")
        .arg("check")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PasswordAuthentication=no")
        .arg("-o")
        .arg("KbdInteractiveAuthentication=no")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=0")
        .arg("--")
        .arg(&connection.host);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_proxy_jump_master(connection: &Connection) -> Result<(), String> {
    let Some(jump) = proxy_jump_mux(connection) else {
        return Ok(());
    };
    if proxy_jump_master_is_alive(connection, &jump) {
        return Ok(());
    }
    if !connection_has_password(connection) {
        return Err("password required: proxy jump master session expired".to_string());
    }

    let _ = std::fs::remove_file(&jump.control_path);
    let mut command = proxy_jump_ssh_command(connection);
    command
        .arg("-M")
        .arg("-N")
        .arg("-f")
        .arg("-o")
        .arg(format!(
            "ConnectTimeout={SSH_OPERATION_CONNECT_TIMEOUT_SECS}"
        ))
        .arg("-o")
        .arg("ClearAllForwardings=yes")
        .arg("-o")
        .arg("ControlMaster=yes")
        .arg("-o")
        .arg(format!("ControlPersist={CONTROL_PERSIST_SECS}"))
        .arg("-o")
        .arg(format!("ControlPath={}", jump.control_path))
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg("-o")
        .arg("ServerAliveCountMax=3");
    append_auth_options(&mut command, connection);
    append_proxy_jump_target_args(&mut command, &jump.target);

    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("failed to spawn ssh master: {err}"))?;
    if output.status.success() && proxy_jump_master_is_alive(connection, &jump) {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(useful_error(&stderr, output.status.code()))
    }
}

fn proxy_jump_master_is_alive(connection: &Connection, jump: &ProxyJumpMux) -> bool {
    let mut command = proxy_jump_ssh_command(connection);
    command
        .arg("-S")
        .arg(&jump.control_path)
        .arg("-O")
        .arg("check")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PasswordAuthentication=no")
        .arg("-o")
        .arg("KbdInteractiveAuthentication=no")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=0");
    append_proxy_jump_target_args(&mut command, &jump.target);

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn proxy_jump_ssh_command(connection: &Connection) -> Command {
    let mut command = Command::new("ssh");
    append_config(&mut command, connection);
    command
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProxyJumpMux {
    target: String,
    control_path: String,
}

fn proxy_jump_mux(connection: &Connection) -> Option<ProxyJumpMux> {
    let config_path = connection.config_path.as_deref();
    let effective = resolve_effective_ssh(&connection.host, config_path)?;
    let proxy_jump = effective.proxy_jump.as_deref()?;
    if proxy_jump.contains(',') {
        return None;
    }

    let target = first_proxy_jump(proxy_jump)?.to_string();
    let control_path = control_path_for_proxy_jump(config_path, &target)?;
    Some(ProxyJumpMux {
        target,
        control_path,
    })
}

fn proxy_jump_mux_command(connection: &Connection) -> Option<String> {
    let jump = proxy_jump_mux(connection)?;
    let mut parts = vec!["ssh".to_string()];
    if let Some(config_path) = connection.config_path.as_deref() {
        parts.push("-F".to_string());
        parts.push(browser::shell_escape(&config_path.to_string_lossy()));
    }

    parts.extend([
        "-o".to_string(),
        format!("ConnectTimeout={SSH_OPERATION_CONNECT_TIMEOUT_SECS}"),
        "-o".to_string(),
        "ClearAllForwardings=yes".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPersist={CONTROL_PERSIST_SECS}"),
        "-o".to_string(),
        format!("ControlPath={}", browser::shell_escape(&jump.control_path)),
    ]);

    append_proxy_reuse_only_auth_parts(&mut parts);
    parts.push("-W".to_string());
    parts.push("'[%h]:%p'".to_string());
    append_proxy_jump_target_parts(&mut parts, &jump.target);
    Some(parts.join(" "))
}

fn append_proxy_reuse_only_auth_parts(parts: &mut Vec<String>) {
    parts.extend([
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "PasswordAuthentication=no".to_string(),
        "-o".to_string(),
        "KbdInteractiveAuthentication=no".to_string(),
        "-o".to_string(),
        "NumberOfPasswordPrompts=0".to_string(),
    ]);
}

fn append_proxy_jump_target_parts(parts: &mut Vec<String>, value: &str) {
    let target = parse_proxy_jump_target(value);
    if let Some(port) = target.port {
        parts.push("-p".to_string());
        parts.push(port.to_string());
    }
    parts.push(browser::shell_escape(&proxy_jump_endpoint(&target)));
}

fn append_proxy_jump_target_args(command: &mut Command, value: &str) {
    let target = parse_proxy_jump_target(value);
    if let Some(port) = target.port {
        command.arg("-p").arg(port.to_string());
    }
    command.arg("--").arg(proxy_jump_endpoint(&target));
}

fn proxy_jump_endpoint(target: &ProxyJumpTarget) -> String {
    let host = if target.host.contains(':') && !target.host.starts_with('[') {
        format!("[{}]", target.host)
    } else {
        target.host.clone()
    };

    target
        .user
        .as_ref()
        .map(|user| format!("{user}@{host}"))
        .unwrap_or(host)
}

fn connection_has_password(connection: &Connection) -> bool {
    connection
        .password
        .as_ref()
        .is_some_and(PasswordHandle::is_available)
}

fn consume_password(connection: &Connection) {
    if let Some(password) = &connection.password {
        password.mark_consumed();
    }
}

fn append_password(command: &mut Command, connection: &Connection) {
    if let Some(password) = &connection.password {
        password.configure_command(command);
    }
}

fn append_auth_options(command: &mut Command, connection: &Connection) {
    if connection_has_password(connection) {
        detach_controlling_terminal(command);
        command
            .arg("-o")
            .arg("BatchMode=no")
            .arg("-o")
            .arg("PasswordAuthentication=yes")
            .arg("-o")
            .arg("KbdInteractiveAuthentication=yes")
            .arg("-o")
            .arg("NumberOfPasswordPrompts=1")
            .arg("-o")
            .arg("PreferredAuthentications=publickey,keyboard-interactive,password");
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
    user: Option<String>,
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
    let (user, without_user) = value
        .rsplit_once('@')
        .map(|(user, host)| (Some(user.to_string()), host))
        .unwrap_or((None, value));

    if let Some(rest) = without_user.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail.strip_prefix(':').and_then(|raw| raw.parse().ok());
        return ProxyJumpTarget {
            user,
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
            user,
            host: host.to_string(),
            port: Some(port),
        };
    }

    ProxyJumpTarget {
        user,
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

fn connection_control_path(connection: &Connection) -> Option<String> {
    let base = mux_base_dir()?;
    let mut hasher = DefaultHasher::new();
    connection.config_path.hash(&mut hasher);
    connection.host.hash(&mut hasher);
    Some(
        base.join(format!("target-{:016x}", hasher.finish()))
            .to_string_lossy()
            .into_owned(),
    )
}

fn control_path_for_proxy_jump(config_path: Option<&Path>, host: &str) -> Option<String> {
    let base = mux_base_dir()?;
    let mut hasher = DefaultHasher::new();
    config_path.hash(&mut hasher);
    host.hash(&mut hasher);
    Some(
        base.join(format!("jump-{:016x}", hasher.finish()))
            .to_string_lossy()
            .into_owned(),
    )
}

fn mux_base_dir() -> Option<PathBuf> {
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

    Some(base)
}

/// Drop the noise OpenSSH prints on stderr (banners, warnings) and keep the
/// lines that actually explain a failure.
pub fn filter_ssh_stderr(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("Warning:"))
        .filter(|line| !line.starts_with("** WARNING:"))
        .filter(|line| !line.starts_with("** This session"))
        .filter(|line| !line.starts_with("** The server"))
        .filter(|line| !line.starts_with("Connection to "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn useful_error(stderr: &str, code: Option<i32>) -> String {
    let filtered = filter_ssh_stderr(stderr);
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
    fn handshake_timeout_tolerates_slow_session_setup() {
        // Busy login nodes have been measured needing well over 30s to
        // answer the subsystem. A tight timeout here makes healthy transfers
        // abandon a connection that was about to come up and reconnect for
        // nothing, so keep plenty of headroom.
        assert!(SFTP_HANDSHAKE_TIMEOUT >= Duration::from_secs(90));
        assert!(handshake_timeout_message().contains("120s"));
    }

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
        assert_eq!(with_user.user.as_deref(), Some("deploy"));
        assert_eq!(with_user.host, "jump.example.com");
        assert_eq!(with_user.port, Some(2200));
        assert_eq!(proxy_jump_endpoint(&with_user), "deploy@jump.example.com");

        let ipv6 = parse_proxy_jump_target("root@[2001:db8::1]:2222");
        assert_eq!(ipv6.user.as_deref(), Some("root"));
        assert_eq!(ipv6.host, "2001:db8::1");
        assert_eq!(ipv6.port, Some(2222));
        assert_eq!(proxy_jump_endpoint(&ipv6), "root@[2001:db8::1]");

        let bare_ipv6 = parse_proxy_jump_target("2001:db8::2");
        assert_eq!(bare_ipv6.user, None);
        assert_eq!(bare_ipv6.host, "2001:db8::2");
        assert_eq!(bare_ipv6.port, None);
    }

    #[test]
    fn proxy_command_reuses_jump_master_without_password_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::write(
            &config,
            "Host web\n  HostName web.internal\n  ProxyJump jump\nHost jump\n  HostName jump.internal\n",
        )
        .unwrap();
        let password = PasswordHandle::new("123456").unwrap();
        let connection = Connection {
            host: "web".to_string(),
            config_path: Some(config),
            password: Some(password),
        };

        let command = proxy_jump_mux_command(&connection).expect("proxy command");

        assert!(command.contains("ControlMaster=auto"));
        assert!(command.contains("PasswordAuthentication=no"));
        assert!(command.contains("KbdInteractiveAuthentication=no"));
        assert!(command.contains("NumberOfPasswordPrompts=0"));
        assert!(command.contains("-W '[%h]:%p' 'jump'"));
        assert!(!command.contains("PasswordAuthentication=yes"));
        assert!(!command.contains("NumberOfPasswordPrompts=1"));
    }

    #[test]
    fn builds_sftp_subsystem_command() {
        let connection = Connection {
            host: "web".to_string(),
            config_path: None,
            password: None,
        };

        let command = ssh_sftp_subsystem_command(&connection);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(command.get_program(), "ssh");
        assert_eq!(
            &args[args.len() - 4..],
            ["-s", "--", "web", "sftp"],
            "subsystem invocation should end with -s -- <host> sftp"
        );
        assert!(args.iter().any(|arg| arg == "BatchMode=yes"));
    }

}
