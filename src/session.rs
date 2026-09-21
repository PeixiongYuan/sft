//! Long-lived SFTP control session, one per host.
//!
//! Directory listings and file operations used to spawn a fresh `sftp`
//! process per request. Opening a session channel costs a remote process
//! spawn plus whatever the server does per session (PAM, LDAP, mounting a
//! network home), which on a busy login node is measured in seconds — paid
//! again on every keystroke that changed directory.
//!
//! Here a worker thread keeps one SFTP session open per host and serves
//! requests over it, so that setup cost is paid once per host. Transfers keep
//! their own session (see [`crate::transfer`]) so a long copy cannot block
//! browsing behind it.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures_core::Stream;
use openssh_sftp_client::{Error as SftpError, Sftp, SftpOptions};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};

use crate::browser::{self, Entry, SortMode};
use crate::remote::{self, Connection};

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const STDERR_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_QUEUE: usize = 32;

/// Whether an error means the session itself is gone (so reconnecting may
/// help) rather than the server refusing one request.
pub fn is_connection_error(err: &SftpError) -> bool {
    !matches!(err, SftpError::SftpError(..))
}

pub fn realpath(connection: &Connection, path: &str) -> Result<String, String> {
    match request(connection, Op::Realpath(path.to_string()))? {
        Output::Path(path) => Ok(path),
        _ => Err("unexpected response from remote session".to_string()),
    }
}

pub fn list_entries(
    connection: &Connection,
    path: &str,
    show_hidden: bool,
    sort: SortMode,
) -> Result<Vec<Entry>, String> {
    let Output::Entries(entries) = request(connection, Op::List(path.to_string()))? else {
        return Err("unexpected response from remote session".to_string());
    };
    Ok(present(entries, show_hidden, sort))
}

/// Apply the view's hidden-file filter and sort order to a raw listing.
fn present(entries: Vec<Entry>, show_hidden: bool, sort: SortMode) -> Vec<Entry> {
    let mut entries = entries
        .into_iter()
        .filter(|entry| show_hidden || !entry.name.starts_with('.'))
        .collect::<Vec<_>>();
    browser::sort_entries(&mut entries, sort);
    entries
}

pub fn create_dir(connection: &Connection, path: &str) -> Result<(), String> {
    request(connection, Op::CreateDir(path.to_string())).map(|_| ())
}

pub fn rename(connection: &Connection, from: &str, to: &str) -> Result<(), String> {
    request(connection, Op::Rename(from.to_string(), to.to_string())).map(|_| ())
}

pub fn remove(connection: &Connection, paths: Vec<String>) -> Result<(), String> {
    request(connection, Op::Remove(paths)).map(|_| ())
}

/// Drop the session for this host, closing its channel. The next request
/// opens a fresh one.
pub fn shutdown(connection: &Connection) {
    let Ok(mut sessions) = SESSIONS.lock() else {
        return;
    };
    sessions.remove(&session_key(connection));
}

enum Op {
    Realpath(String),
    List(String),
    CreateDir(String),
    Rename(String, String),
    Remove(Vec<String>),
}

impl Op {
    fn describe(&self) -> String {
        match self {
            Self::Realpath(path) => format!("resolve {path}"),
            Self::List(path) => format!("list {path}"),
            Self::CreateDir(path) => format!("create directory {path}"),
            Self::Rename(from, _) => format!("rename {from}"),
            Self::Remove(paths) => match paths.as_slice() {
                [only] => format!("delete {only}"),
                _ => format!("delete {} items", paths.len()),
            },
        }
    }
}

enum Output {
    Path(String),
    Entries(Vec<Entry>),
    Done,
}

struct Job {
    connection: Connection,
    op: Op,
    reply: oneshot::Sender<Result<Output, String>>,
}

/// Builds the command that opens a session channel. Blocking, because
/// bringing up the ssh master shells out. Injectable so tests can drive a
/// local `sftp-server` instead of a host.
type OpenCommand =
    Arc<dyn Fn(&Connection) -> Result<std::process::Command, String> + Send + Sync>;

fn ssh_subsystem_command() -> OpenCommand {
    Arc::new(|connection: &Connection| {
        remote::ensure_connection_master(connection)?;
        Ok(remote::ssh_sftp_subsystem_command(connection))
    })
}

static SESSIONS: LazyLock<Mutex<HashMap<String, mpsc::Sender<Job>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn session_key(connection: &Connection) -> String {
    let config = connection
        .config_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{config}\u{0}{}", connection.host)
}

fn sender_for(connection: &Connection) -> Result<mpsc::Sender<Job>, String> {
    let mut sessions = SESSIONS
        .lock()
        .map_err(|_| "remote session registry is poisoned".to_string())?;
    let key = session_key(connection);
    if let Some(tx) = sessions.get(&key)
        && !tx.is_closed()
    {
        return Ok(tx.clone());
    }

    let (tx, rx) = mpsc::channel(REQUEST_QUEUE);
    std::thread::Builder::new()
        .name(format!("sft-sftp-{}", connection.host))
        .spawn(move || worker(rx, ssh_subsystem_command()))
        .map_err(|err| format!("failed to start remote session: {err}"))?;
    sessions.insert(key, tx.clone());
    Ok(tx)
}

fn request(connection: &Connection, op: Op) -> Result<Output, String> {
    submit(&sender_for(connection)?, connection, op)
}

/// Requests are issued from the app's worker threads, never from inside a
/// runtime, so blocking on the reply here is safe.
fn submit(tx: &mpsc::Sender<Job>, connection: &Connection, op: Op) -> Result<Output, String> {
    let context = op.describe();
    let (reply, response) = oneshot::channel();
    tx.blocking_send(Job {
        connection: connection.clone(),
        op,
        reply,
    })
    .map_err(|_| format!("{context}: remote session is not running"))?;

    response
        .blocking_recv()
        .map_err(|_| format!("{context}: remote session stopped"))?
}

fn worker(rx: mpsc::Receiver<Job>, open: OpenCommand) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(serve(rx, open));
}

async fn serve(mut rx: mpsc::Receiver<Job>, open: OpenCommand) {
    let mut channel: Option<Channel> = None;
    while let Some(job) = rx.recv().await {
        let result = dispatch(&mut channel, &open, &job.connection, &job.op).await;
        let _ = job.reply.send(result);
    }
    if let Some(channel) = channel {
        channel.close().await;
    }
}

async fn dispatch(
    channel: &mut Option<Channel>,
    open: &OpenCommand,
    connection: &Connection,
    op: &Op,
) -> Result<Output, String> {
    let context = op.describe();
    // Two passes at most: a session that died while idle fails the first
    // attempt, and the reconnect makes that invisible to the caller.
    for attempt in 0..2 {
        if channel.is_none() {
            *channel = Some(
                connect(connection, open)
                    .await
                    .map_err(|err| format!("{context}: {err}"))?,
            );
        }
        let open = channel.as_ref().expect("connected above");
        match run(&open.sftp, op).await {
            Ok(output) => return Ok(output),
            Err(err) => {
                let retryable = is_connection_error(&err);
                if retryable && let Some(open) = channel.take() {
                    open.abort().await;
                }
                if !retryable || attempt == 1 {
                    return Err(format!("{context}: {err}"));
                }
            }
        }
    }
    Err(format!("{context}: remote session could not be restored"))
}

async fn run(sftp: &Sftp, op: &Op) -> Result<Output, SftpError> {
    let mut fs = sftp.fs();
    match op {
        Op::Realpath(path) => {
            let resolved = fs.canonicalize(path).await?;
            Ok(Output::Path(resolved.to_string_lossy().into_owned()))
        }
        Op::List(path) => read_dir(sftp, path).await.map(Output::Entries),
        Op::CreateDir(path) => fs.create_dir(path).await.map(|()| Output::Done),
        Op::Rename(from, to) => fs.rename(from, to).await.map(|()| Output::Done),
        Op::Remove(paths) => {
            for path in paths {
                remove_tree(sftp, path).await?;
            }
            Ok(Output::Done)
        }
    }
}

/// Every entry in `path`, unsorted and unfiltered. Readdir already carries
/// the attributes, so this costs one round trip regardless of entry count —
/// no per-file stat.
async fn read_dir(sftp: &Sftp, path: &str) -> Result<Vec<Entry>, SftpError> {
    let dir = sftp.fs().open_dir(path).await?;
    let mut read_dir = std::pin::pin!(dir.read_dir());
    let mut entries = Vec::new();
    while let Some(entry) = std::future::poll_fn(|cx| read_dir.as_mut().poll_next(cx)).await {
        let entry = entry?;
        let name = entry.filename().to_string_lossy().into_owned();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let meta = entry.metadata();
        // Symlinks report as symlinks, not as their target, matching what
        // the old `ls -l` parsing produced.
        let is_dir = meta.file_type().is_some_and(|kind| kind.is_dir());
        entries.push(Entry {
            name,
            is_dir,
            size: (!is_dir).then(|| meta.len().unwrap_or(0)),
            modified: meta.modified().map(|time| i64::from(time.into_raw())),
        });
    }
    Ok(entries)
}

/// Delete `path`, recursing into real directories. Symlinks are unlinked
/// rather than followed, like `rm -rf`.
async fn remove_tree(sftp: &Sftp, path: &str) -> Result<(), SftpError> {
    let mut fs = sftp.fs();
    let meta = fs.symlink_metadata(path).await?;
    if !meta.file_type().is_some_and(|kind| kind.is_dir()) {
        return fs.remove_file(path).await;
    }

    // Collect directories top-down, removing files as we go, then unwind so
    // children are always gone before their parent.
    let mut pending = vec![path.to_string()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in read_dir(sftp, &directory).await? {
            let child = browser::join_remote_path(&directory, &entry.name);
            if entry.is_dir {
                pending.push(child);
            } else {
                fs.remove_file(&child).await?;
            }
        }
        directories.push(directory);
    }
    for directory in directories.into_iter().rev() {
        fs.remove_dir(&directory).await?;
    }
    Ok(())
}

struct Channel {
    sftp: Sftp,
    child: tokio::process::Child,
}

impl Channel {
    async fn close(self) {
        let Self { sftp, mut child } = self;
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, sftp.close()).await;
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
    }

    /// Tear down a session already known to be broken. A graceful close would
    /// wait on a peer that is not answering.
    async fn abort(self) {
        let Self { sftp, mut child } = self;
        drop(sftp);
        let _ = child.start_kill();
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
    }
}

async fn connect(connection: &Connection, open: &OpenCommand) -> Result<Channel, String> {
    let open = Arc::clone(open);
    let for_open = connection.clone();
    let built = tokio::task::spawn_blocking(move || open(&for_open))
        .await
        .map_err(|err| format!("failed to prepare ssh session: {err}"))??;

    let mut command = tokio::process::Command::from(built);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to spawn ssh: {err}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open ssh stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open ssh stdout".to_string())?;
    let stderr = child.stderr.take();

    match tokio::time::timeout(
        remote::SFTP_HANDSHAKE_TIMEOUT,
        Sftp::new(stdin, stdout, SftpOptions::new()),
    )
    .await
    {
        Ok(Ok(sftp)) => Ok(Channel { sftp, child }),
        // ssh reports auth failures on stderr, and the app reads those
        // strings to decide whether to ask for a password.
        Ok(Err(err)) => {
            Err(with_stderr(&format!("failed to start sftp session: {err}"), stderr).await)
        }
        Err(_) => Err(with_stderr(&remote::handshake_timeout_message(), stderr).await),
    }
}

async fn with_stderr<R>(message: &str, stderr: Option<R>) -> String
where
    R: AsyncReadExt + Unpin,
{
    let Some(mut stderr) = stderr else {
        return message.to_string();
    };
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(STDERR_TIMEOUT, stderr.read_to_end(&mut buf)).await;
    let text = remote::filter_ssh_stderr(&String::from_utf8_lossy(&buf));
    if text.is_empty() {
        message.to_string()
    } else {
        format!("{message}\n{text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Tests drive a local `sftp-server` over stdio, exercising the real
    /// session machinery without an ssh host.
    fn sftp_server_path() -> Option<&'static str> {
        [
            "/usr/libexec/sftp-server",
            "/usr/lib/openssh/sftp-server",
            "/usr/lib/ssh/sftp-server",
            "/usr/libexec/openssh/sftp-server",
        ]
        .into_iter()
        .find(|path| Path::new(path).exists())
    }

    fn test_connection() -> Connection {
        Connection {
            host: "test".to_string(),
            config_path: None,
            password: None,
        }
    }

    /// A worker fed by `build`, plus the count of channels it has opened.
    struct Harness {
        tx: mpsc::Sender<Job>,
        opened: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(build: impl Fn() -> Command + Send + Sync + 'static) -> Self {
            let opened = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&opened);
            let (tx, rx) = mpsc::channel(REQUEST_QUEUE);
            std::thread::spawn(move || {
                worker(
                    rx,
                    Arc::new(move |_: &Connection| {
                        counter.fetch_add(1, Ordering::Relaxed);
                        Ok(build())
                    }),
                )
            });
            Self { tx, opened }
        }

        fn run(&self, op: Op) -> Result<Output, String> {
            submit(&self.tx, &test_connection(), op)
        }

        /// Mirrors [`list_entries`], which cannot be used directly because it
        /// resolves the session through the global registry.
        fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
            match self.run(Op::List(path.to_string()))? {
                Output::Entries(entries) => Ok(present(entries, false, SortMode::Name)),
                _ => Err("unexpected response".to_string()),
            }
        }

        fn channels_opened(&self) -> usize {
            self.opened.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn one_channel_serves_many_operations() {
        let Some(server) = sftp_server_path() else {
            eprintln!("skipping: no local sftp-server binary");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        std::fs::write(dir.path().join("report.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();

        let harness = Harness::new(move || Command::new(server));

        let Output::Path(resolved) = harness.run(Op::Realpath(root.clone())).unwrap() else {
            panic!("expected a path");
        };
        assert!(resolved.ends_with(dir.path().file_name().unwrap().to_str().unwrap()));

        // Directories sort ahead of files, dotfiles are filtered out, and
        // sizes come from readdir attributes rather than a parsed `ls` column.
        for _ in 0..5 {
            let entries = harness.list(&root).unwrap();
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| (entry.name.as_str(), entry.is_dir, entry.size))
                    .collect::<Vec<_>>(),
                vec![("data", true, None), ("report.txt", false, Some(5))]
            );
            assert!(entries[1].modified.is_some());
        }

        harness
            .run(Op::CreateDir(format!("{root}/fresh")))
            .unwrap();
        harness
            .run(Op::Rename(
                format!("{root}/report.txt"),
                format!("{root}/renamed.txt"),
            ))
            .unwrap();
        assert!(dir.path().join("fresh").is_dir());
        assert!(dir.path().join("renamed.txt").is_file());

        assert_eq!(
            harness.channels_opened(),
            1,
            "every operation should reuse the same session channel"
        );
    }

    #[test]
    fn remove_deletes_trees_without_following_symlinks() {
        let Some(server) = sftp_server_path() else {
            eprintln!("skipping: no local sftp-server binary");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), b"keep").unwrap();

        let tree = dir.path().join("tree");
        std::fs::create_dir_all(tree.join("nested/deep")).unwrap();
        std::fs::write(tree.join("top.txt"), b"a").unwrap();
        std::fs::write(tree.join("nested/deep/leaf.txt"), b"b").unwrap();
        std::fs::write(dir.path().join("loose.txt"), b"c").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tree.join("link")).unwrap();

        let harness = Harness::new(move || Command::new(server));
        harness
            .run(Op::Remove(vec![
                tree.to_str().unwrap().to_string(),
                dir.path().join("loose.txt").to_str().unwrap().to_string(),
            ]))
            .unwrap();

        assert!(!tree.exists());
        assert!(!dir.path().join("loose.txt").exists());
        assert!(
            outside.path().join("keep.txt").exists(),
            "a symlinked directory must be unlinked, not emptied"
        );
    }

    #[test]
    fn server_side_errors_do_not_reconnect() {
        let Some(server) = sftp_server_path() else {
            eprintln!("skipping: no local sftp-server binary");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing").to_str().unwrap().to_string();

        let harness = Harness::new(move || Command::new(server));
        let err = harness.list(&missing).unwrap_err();

        assert!(err.contains("missing"), "unexpected error: {err}");
        assert_eq!(
            harness.channels_opened(),
            1,
            "a rejected request must not tear down a healthy session"
        );
    }

    #[test]
    fn a_dead_channel_is_reconnected_transparently() {
        let Some(server) = sftp_server_path() else {
            eprintln!("skipping: no local sftp-server binary");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_string();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();

        // `dd bs=1` forwards byte by byte and then closes the stream, so the
        // session dies partway through the run and has to be rebuilt. (Larger
        // block sizes buffer and would stall the handshake instead.)
        let harness = Harness::new(move || {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(format!("dd bs=1 count=400 2>/dev/null | {server}"));
            command
        });

        for _ in 0..25 {
            let entries = harness.list(&root).unwrap();
            assert_eq!(entries.len(), 1);
        }

        assert!(
            harness.channels_opened() > 1,
            "the broken session should have been rebuilt"
        );
    }

    #[test]
    fn session_key_separates_hosts_and_configs() {
        let base = Connection {
            host: "web".to_string(),
            config_path: Some("/home/me/.ssh/config".into()),
            password: None,
        };
        let other_host = Connection {
            host: "db".to_string(),
            ..base.clone()
        };
        let other_config = Connection {
            config_path: Some("/tmp/other".into()),
            ..base.clone()
        };

        assert_eq!(session_key(&base), session_key(&base.clone()));
        assert_ne!(session_key(&base), session_key(&other_host));
        assert_ne!(session_key(&base), session_key(&other_config));
    }

    #[test]
    fn transport_failures_count_as_connection_errors() {
        assert!(is_connection_error(&SftpError::BackgroundTaskFailure(
            &"read task"
        )));
    }
}
