//! In-process SFTP transfer engine.
//!
//! Instead of shelling out to the `sftp` CLI, transfers speak the SFTP wire
//! protocol directly over an `ssh -s sftp` subsystem channel that reuses the
//! existing connection master. The copy loops live in this process, so
//! progress is exact byte counts and needs no remote polling.
//!
//! Connection-level failures are retried automatically: the transfer state
//! survives across attempts, uploads resume from the bytes already on the
//! server (like `reput`), and striped downloads resume from the chunks each
//! stripe already completed. Server-reported SFTP errors (permission denied,
//! missing files) fail immediately.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::BytesMut;
use futures_core::Stream;
use openssh_sftp_client::file::TokioCompatFile;
use openssh_sftp_client::fs::Fs;
use openssh_sftp_client::metadata::{MetaData, MetaDataBuilder, Permissions, RawPermissions};
use openssh_sftp_client::{Sftp, SftpOptions, UnixTimeStamp};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::task::JoinSet;

use crate::browser::{self, Pane};
use crate::remote::{self, Connection};

const UPLOAD_CHUNK: usize = 256 * 1024;
/// OpenSSH's negotiated maximum read length (255KB). Sizing chunks to it lets
/// each striped chunk complete in a single read request; servers with a
/// smaller limit still work, just with follow-up reads per chunk.
const DOWNLOAD_CHUNK: u64 = 255 * 1024;
const DOWNLOAD_STRIPES: u64 = 8;
/// Files at least this large are downloaded with several concurrent handles
/// so throughput is not capped at one request per round trip.
const STRIPE_THRESHOLD: u64 = 2 * DOWNLOAD_CHUNK;
/// Upper bound on upload bytes queued before waiting for server acks.
const WRITE_PIPELINE_LIMIT: usize = 4 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const CHILD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Transfers follow symlinks, so bound directory nesting to catch link loops.
const MAX_SCAN_DEPTH: u32 = 64;
/// Give up after this many consecutive attempts without any forward progress.
const MAX_STALLED_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone)]
pub enum TransferUpdate {
    Progress {
        transferred_bytes: u64,
        total_bytes: Option<u64>,
    },
    Retrying {
        attempt: u32,
        error: String,
    },
}

/// Internal error carrying whether reconnecting could plausibly help.
struct TransferError {
    retryable: bool,
    message: String,
}

impl TransferError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            retryable: false,
            message: message.into(),
        }
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            message: message.into(),
        }
    }

    fn context(self, context: impl std::fmt::Display) -> Self {
        Self {
            retryable: self.retryable,
            message: format!("{context}: {}", self.message),
        }
    }
}

/// Server-reported SFTP status errors (no such file, permission denied, ...)
/// will not be fixed by reconnecting; everything else is connection-level.
fn sftp_error(err: openssh_sftp_client::Error) -> TransferError {
    TransferError {
        retryable: !matches!(err, openssh_sftp_client::Error::SftpError(..)),
        message: err.to_string(),
    }
}

fn local_error(err: std::io::Error) -> TransferError {
    TransferError::fatal(err.to_string())
}

pub fn run_transfer(
    connection: &Connection,
    source: Pane,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
    progress: impl FnMut(TransferUpdate) + Send,
) -> Result<(), String> {
    run_transfer_with(
        || {
            remote::ensure_connection_master(connection)?;
            Ok(remote::ssh_sftp_subsystem_command(connection))
        },
        source,
        local_path,
        remote_path,
        names,
        progress,
    )
}

pub fn run_transfer_with(
    mut make_command: impl FnMut() -> Result<std::process::Command, String>,
    source: Pane,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
    progress: impl FnMut(TransferUpdate) + Send,
) -> Result<(), String> {
    if names.is_empty() {
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start async runtime: {err}"))?;

    let mut reporter = Reporter::new(progress);
    let mut state = match source {
        Pane::Local => JobState::Upload(UploadState::default()),
        Pane::Remote => JobState::Download(DownloadState::default()),
    };
    let mut stalled_attempts = 0u32;
    let mut last_observed = 0u64;

    loop {
        let result = match make_command() {
            Ok(command) => runtime.block_on(attempt_transfer(
                command,
                local_path,
                remote_path,
                names,
                &mut state,
                &mut reporter,
            )),
            Err(err) => Err(TransferError::retryable(err)),
        };

        let err = match result {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };

        let observed = reporter.observed();
        if observed > last_observed {
            stalled_attempts = 1;
        } else {
            stalled_attempts += 1;
        }
        last_observed = observed;

        if !err.retryable || stalled_attempts >= MAX_STALLED_ATTEMPTS {
            return Err(err.message);
        }
        reporter.notify_retry(stalled_attempts, &err.message);
        std::thread::sleep(retry_backoff(stalled_attempts));
    }
}

fn retry_backoff(stalled_attempts: u32) -> Duration {
    let seconds = 1u64 << (stalled_attempts - 1).min(3);
    if cfg!(test) {
        Duration::from_millis(seconds * 100)
    } else {
        Duration::from_secs(seconds)
    }
}

async fn attempt_transfer(
    command: std::process::Command,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
    state: &mut JobState,
    reporter: &mut Reporter<impl FnMut(TransferUpdate)>,
) -> Result<(), TransferError> {
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| TransferError::retryable(format!("failed to spawn ssh: {err}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| TransferError::retryable("failed to open ssh stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| TransferError::retryable("failed to open ssh stdout"))?;
    let stderr_task = child.stderr.take().map(|mut stderr| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        })
    });

    let result = async {
        let options = SftpOptions::new().tokio_compat_file_write_limit(
            WRITE_PIPELINE_LIMIT.try_into().expect("nonzero limit"),
        );
        let sftp = tokio::time::timeout(HANDSHAKE_TIMEOUT, Sftp::new(stdin, stdout, options))
            .await
            .map_err(|_| TransferError::retryable("sftp handshake timed out"))?
            .map_err(|err| {
                TransferError::retryable(format!("failed to start sftp session: {err}"))
            })?;
        let sftp = Arc::new(sftp);

        let transferred = match state {
            JobState::Upload(upload_state) => {
                attempt_upload(&sftp, local_path, remote_path, names, upload_state, reporter)
                    .await
            }
            JobState::Download(download_state) => {
                attempt_download(
                    &sftp,
                    local_path,
                    remote_path,
                    names,
                    download_state,
                    reporter,
                )
                .await
            }
        };

        let closed = match Arc::try_unwrap(sftp) {
            Ok(sftp) => tokio::time::timeout(CHILD_CLEANUP_TIMEOUT, sftp.close())
                .await
                .unwrap_or(Ok(()))
                .map_err(|err| {
                    TransferError::retryable(format!("failed to close sftp session: {err}"))
                }),
            Err(_) => Ok(()),
        };
        transferred.and(closed)
    }
    .await;

    match result {
        Ok(()) => {
            let _ = tokio::time::timeout(CHILD_CLEANUP_TIMEOUT, child.wait()).await;
            Ok(())
        }
        Err(err) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(CHILD_CLEANUP_TIMEOUT, child.wait()).await;
            let stderr = match stderr_task {
                Some(task) => tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .ok()
                    .and_then(|joined| joined.ok())
                    .unwrap_or_default(),
                None => String::new(),
            };
            let filtered = remote::filter_ssh_stderr(&stderr);
            if filtered.is_empty() {
                Err(err)
            } else {
                let retryable = err.retryable;
                Err(TransferError {
                    retryable,
                    message: format!("{}\n{filtered}", err.message),
                })
            }
        }
    }
}

struct Reporter<F> {
    emit: F,
    counter: Arc<AtomicU64>,
    total: Option<u64>,
    max_seen: u64,
    last_at: Instant,
    last_value: Option<u64>,
}

impl<F: FnMut(TransferUpdate)> Reporter<F> {
    fn new(emit: F) -> Self {
        Self {
            emit,
            counter: Arc::new(AtomicU64::new(0)),
            total: None,
            max_seen: 0,
            last_at: Instant::now(),
            last_value: None,
        }
    }

    fn add(&mut self, bytes: u64) {
        self.counter.fetch_add(bytes, Ordering::Relaxed);
        self.tick(false);
    }

    /// Reset the live counter to the bytes known to be persisted, at the
    /// start of a retry attempt.
    fn set(&mut self, bytes: u64) {
        self.counter.store(bytes, Ordering::Relaxed);
    }

    fn observed(&mut self) -> u64 {
        let value = self.counter.load(Ordering::Relaxed);
        self.max_seen = self.max_seen.max(value);
        self.max_seen
    }

    fn tick(&mut self, force: bool) {
        let value = self.counter.load(Ordering::Relaxed);
        self.max_seen = self.max_seen.max(value);
        if !force
            && (self.last_value == Some(value) || self.last_at.elapsed() < PROGRESS_INTERVAL)
        {
            return;
        }
        (self.emit)(TransferUpdate::Progress {
            transferred_bytes: value,
            total_bytes: self.total,
        });
        self.last_at = Instant::now();
        self.last_value = Some(value);
    }

    fn notify_retry(&mut self, attempt: u32, error: &str) {
        (self.emit)(TransferUpdate::Retrying {
            attempt,
            error: error.to_string(),
        });
    }
}

enum JobState {
    Upload(UploadState),
    Download(DownloadState),
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

struct LocalFileJob {
    src: PathBuf,
    dst: String,
    size: u64,
}

struct LocalDirJob {
    src: PathBuf,
    dst: String,
}

struct UploadPlan {
    dirs: Vec<LocalDirJob>,
    files: Vec<LocalFileJob>,
    total: u64,
}

#[derive(Default)]
struct UploadState {
    plan: Option<UploadPlan>,
    done: Vec<bool>,
    started: Vec<bool>,
    dirs_created: bool,
}

async fn attempt_upload(
    sftp: &Arc<Sftp>,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
    state: &mut UploadState,
    reporter: &mut Reporter<impl FnMut(TransferUpdate)>,
) -> Result<(), TransferError> {
    if state.plan.is_none() {
        let mut plan = UploadPlan {
            dirs: Vec::new(),
            files: Vec::new(),
            total: 0,
        };
        for name in names {
            scan_local(
                local_path.join(name),
                browser::join_remote_path(remote_path, name),
                &mut plan,
                0,
            )?;
        }
        state.done = vec![false; plan.files.len()];
        state.started = vec![false; plan.files.len()];
        state.plan = Some(plan);
    }
    let UploadState {
        plan,
        done,
        started,
        dirs_created,
    } = state;
    let plan = plan.as_ref().expect("upload plan scanned");
    reporter.total = Some(plan.total);

    let persisted: u64 = plan
        .files
        .iter()
        .zip(done.iter())
        .filter(|(_, done)| **done)
        .map(|(file, _)| file.size)
        .sum();
    reporter.set(persisted);
    reporter.tick(true);

    let mut fs = sftp.fs();
    if !*dirs_created {
        for dir in &plan.dirs {
            ensure_remote_dir(&mut fs, &dir.dst).await?;
        }
        *dirs_created = true;
    }

    for (idx, file) in plan.files.iter().enumerate() {
        if done[idx] {
            continue;
        }
        // Resume from what already reached the server, but only for files a
        // previous attempt started; untouched files truncate as usual.
        let resume = if started[idx] {
            remote_resume_offset(&mut fs, &file.dst, file.size).await?
        } else {
            0
        };
        started[idx] = true;
        if resume == file.size && file.size > 0 {
            reporter.add(file.size);
        } else {
            upload_file(sftp, file, resume, reporter)
                .await
                .map_err(|err| err.context(format_args!("upload {}", file.src.display())))?;
        }
        if let Some(meta) = std::fs::metadata(&file.src).ok().and_then(local_meta_to_sftp) {
            let _ = fs.set_metadata(&file.dst, meta).await;
        }
        done[idx] = true;
    }
    // Preserve directory attributes last (children first) like `put -p` does.
    for dir in plan.dirs.iter().rev() {
        if let Some(meta) = std::fs::metadata(&dir.src).ok().and_then(local_meta_to_sftp) {
            let _ = fs.set_metadata(&dir.dst, meta).await;
        }
    }

    reporter.tick(true);
    Ok(())
}

async fn remote_resume_offset(
    fs: &mut Fs,
    dst: &str,
    size: u64,
) -> Result<u64, TransferError> {
    match fs.metadata(dst).await {
        Ok(meta) if meta.file_type().is_some_and(|kind| kind.is_file()) => {
            let len = meta.len().unwrap_or(0);
            Ok(if len <= size { len } else { 0 })
        }
        Ok(_) => Ok(0),
        Err(err) => {
            let err = sftp_error(err);
            if err.retryable {
                Err(err.context(format_args!("could not inspect {dst}")))
            } else {
                // Missing file (or similar): start the file over.
                Ok(0)
            }
        }
    }
}

fn scan_local(
    src: PathBuf,
    dst: String,
    plan: &mut UploadPlan,
    depth: u32,
) -> Result<(), TransferError> {
    let meta = std::fs::metadata(&src)
        .map_err(|err| TransferError::fatal(format!("could not inspect {}: {err}", src.display())))?;
    if meta.is_file() {
        plan.total = plan.total.saturating_add(meta.len());
        plan.files.push(LocalFileJob {
            src,
            dst,
            size: meta.len(),
        });
        return Ok(());
    }
    if !meta.is_dir() {
        // Sockets, fifos, and other specials cannot travel over SFTP.
        return Ok(());
    }
    if depth >= MAX_SCAN_DEPTH {
        return Err(TransferError::fatal(format!(
            "directory nesting too deep at {} (symlink loop?)",
            src.display()
        )));
    }

    plan.dirs.push(LocalDirJob {
        src: src.clone(),
        dst: dst.clone(),
    });
    let entries = std::fs::read_dir(&src).map_err(|err| {
        TransferError::fatal(format!("could not read directory {}: {err}", src.display()))
    })?;
    for entry in entries {
        let entry = entry
            .map_err(|err| TransferError::fatal(format!("could not read directory entry: {err}")))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        scan_local(
            entry.path(),
            browser::join_remote_path(&dst, &name),
            plan,
            depth + 1,
        )?;
    }
    Ok(())
}

async fn ensure_remote_dir(fs: &mut Fs, path: &str) -> Result<(), TransferError> {
    let created = fs.create_dir(path).await;
    if created.is_ok() {
        return Ok(());
    }
    match fs.metadata(path).await {
        Ok(meta) if meta.file_type().is_some_and(|kind| kind.is_dir()) => Ok(()),
        _ => Err(sftp_error(created.expect_err("checked above"))
            .context(format_args!("could not create remote directory {path}"))),
    }
}

async fn upload_file(
    sftp: &Arc<Sftp>,
    job: &LocalFileJob,
    resume: u64,
    reporter: &mut Reporter<impl FnMut(TransferUpdate)>,
) -> Result<(), TransferError> {
    let mut local = tokio::fs::File::open(&job.src).await.map_err(local_error)?;

    let mut remote_file = if resume > 0 {
        sftp.options()
            .write(true)
            .create(true)
            .open(&job.dst)
            .await
            .map_err(sftp_error)?
    } else {
        sftp.create(&job.dst).await.map_err(sftp_error)?
    };
    if resume > 0 {
        local
            .seek(std::io::SeekFrom::Start(resume))
            .await
            .map_err(local_error)?;
        remote_file
            .seek(std::io::SeekFrom::Start(resume))
            .await
            .map_err(|err| TransferError::retryable(err.to_string()))?;
        reporter.add(resume);
    }
    let compat = TokioCompatFile::new(remote_file);
    tokio::pin!(compat);

    let mut buf = vec![0u8; UPLOAD_CHUNK];
    loop {
        let n = local.read(&mut buf).await.map_err(local_error)?;
        if n == 0 {
            break;
        }
        compat
            .write_all(&buf[..n])
            .await
            .map_err(|err| TransferError::retryable(err.to_string()))?;
        reporter.add(n as u64);
    }
    compat
        .flush()
        .await
        .map_err(|err| TransferError::retryable(err.to_string()))?;
    Ok(())
}

fn local_meta_to_sftp(meta: std::fs::Metadata) -> Option<MetaData> {
    let mut builder = MetaDataBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o7777;
        builder.permissions(Permissions::from_raw(RawPermissions::from_bits_truncate(
            mode,
        )));
    }
    let modified = meta.modified().ok().and_then(|time| UnixTimeStamp::new(time).ok())?;
    let accessed = meta
        .accessed()
        .ok()
        .and_then(|time| UnixTimeStamp::new(time).ok())
        .unwrap_or(modified);
    builder.time(accessed, modified);
    Some(builder.create())
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct RemoteAttrs {
    mode: Option<u32>,
    modified: Option<SystemTime>,
}

impl RemoteAttrs {
    fn from_meta(meta: &MetaData) -> Self {
        Self {
            mode: meta
                .permissions()
                .map(|perm| perm.as_raw().bits() & 0o7777),
            modified: meta.modified().map(UnixTimeStamp::as_system_time),
        }
    }

    fn apply(&self, file: &std::fs::File) {
        #[cfg(unix)]
        if let Some(mode) = self.mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(mode));
        }
        if let Some(modified) = self.modified {
            let _ = file.set_modified(modified);
        }
    }

    fn apply_to_path(&self, path: &Path) {
        if let Ok(file) = std::fs::File::open(path) {
            self.apply(&file);
        }
    }
}

struct RemoteFileJob {
    src: String,
    dst: PathBuf,
    size: u64,
    attrs: RemoteAttrs,
}

struct RemoteDirJob {
    dst: PathBuf,
    attrs: RemoteAttrs,
}

struct DownloadPlan {
    dirs: Vec<RemoteDirJob>,
    files: Vec<RemoteFileJob>,
    total: u64,
}

/// Resume bookkeeping for one striped file: each stripe's next chunk index.
/// The chunk size is captured when the transfer first starts so offsets stay
/// stable across reconnects.
struct StripeProgress {
    chunk_size: u64,
    next: Vec<AtomicU64>,
}

impl StripeProgress {
    fn new(chunk_size: u64) -> Self {
        Self {
            chunk_size,
            next: (0..DOWNLOAD_STRIPES).map(AtomicU64::new).collect(),
        }
    }

    fn completed_bytes(&self, size: u64) -> u64 {
        let mut bytes = 0u64;
        for (stripe, next) in self.next.iter().enumerate() {
            let next = next.load(Ordering::Relaxed);
            let mut chunk = stripe as u64;
            while chunk < next {
                let start = chunk * self.chunk_size;
                if start < size {
                    bytes += self.chunk_size.min(size - start);
                }
                chunk += DOWNLOAD_STRIPES;
            }
        }
        bytes
    }
}

#[derive(Default)]
struct DownloadState {
    plan: Option<DownloadPlan>,
    done: Vec<bool>,
    started: Vec<bool>,
    stripes: HashMap<usize, Arc<StripeProgress>>,
    dirs_created: bool,
}

async fn attempt_download(
    sftp: &Arc<Sftp>,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
    state: &mut DownloadState,
    reporter: &mut Reporter<impl FnMut(TransferUpdate)>,
) -> Result<(), TransferError> {
    if state.plan.is_none() {
        let mut fs = sftp.fs();
        let plan = scan_remote(sftp, &mut fs, local_path, remote_path, names).await?;
        state.done = vec![false; plan.files.len()];
        state.started = vec![false; plan.files.len()];
        state.plan = Some(plan);
    }
    let DownloadState {
        plan,
        done,
        started,
        stripes,
        dirs_created,
    } = state;
    let plan = plan.as_ref().expect("download plan scanned");
    reporter.total = Some(plan.total);

    let mut persisted: u64 = plan
        .files
        .iter()
        .zip(done.iter())
        .filter(|(_, done)| **done)
        .map(|(file, _)| file.size)
        .sum();
    for (idx, progress) in stripes.iter() {
        if !done[*idx] {
            persisted += progress.completed_bytes(plan.files[*idx].size);
        }
    }
    reporter.set(persisted);
    reporter.tick(true);

    if !*dirs_created {
        for dir in &plan.dirs {
            std::fs::create_dir_all(&dir.dst).map_err(|err| {
                TransferError::fatal(format!("could not create {}: {err}", dir.dst.display()))
            })?;
        }
        *dirs_created = true;
    }

    for (idx, file) in plan.files.iter().enumerate() {
        if done[idx] {
            continue;
        }
        let resuming = started[idx];
        started[idx] = true;
        if cfg!(unix) && file.size >= STRIPE_THRESHOLD {
            download_file_striped(sftp, file, stripes, idx, reporter)
                .await
                .map_err(|err| err.context(format_args!("download {}", file.src)))?;
            stripes.remove(&idx);
        } else {
            download_file_sequential(sftp, file, resuming, reporter)
                .await
                .map_err(|err| err.context(format_args!("download {}", file.src)))?;
        }
        done[idx] = true;
    }
    for dir in plan.dirs.iter().rev() {
        dir.attrs.apply_to_path(&dir.dst);
    }

    reporter.tick(true);
    Ok(())
}

async fn scan_remote(
    sftp: &Arc<Sftp>,
    fs: &mut Fs,
    local_path: &Path,
    remote_path: &str,
    names: &[String],
) -> Result<DownloadPlan, TransferError> {
    let mut plan = DownloadPlan {
        dirs: Vec::new(),
        files: Vec::new(),
        total: 0,
    };
    let mut pending: Vec<(String, PathBuf, u32)> = Vec::new();

    for name in names {
        let src = browser::join_remote_path(remote_path, name);
        let dst = local_path.join(name);
        let meta = fs
            .metadata(&src)
            .await
            .map_err(|err| sftp_error(err).context(format_args!("could not inspect {src}")))?;
        record_remote_entry(&mut plan, &mut pending, src, dst, &meta, 0);
    }

    while let Some((dir_src, dir_dst, depth)) = pending.pop() {
        if depth >= MAX_SCAN_DEPTH {
            return Err(TransferError::fatal(format!(
                "directory nesting too deep at {dir_src} (symlink loop?)"
            )));
        }
        let dir = fs.open_dir(&dir_src).await.map_err(|err| {
            sftp_error(err).context(format_args!("could not open directory {dir_src}"))
        })?;
        let mut read_dir = std::pin::pin!(dir.read_dir());
        while let Some(entry) =
            std::future::poll_fn(|cx| read_dir.as_mut().poll_next(cx)).await
        {
            let entry = entry.map_err(|err| {
                sftp_error(err).context(format_args!("could not list directory {dir_src}"))
            })?;
            let name = entry.filename().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            let src = browser::join_remote_path(&dir_src, &name);
            let dst = dir_dst.join(&name);
            let meta = entry.metadata();
            let is_symlink = meta.file_type().is_none_or(|kind| kind.is_symlink());
            if is_symlink {
                // Follow the link server-side, like `get -R` does.
                match sftp.fs().metadata(&src).await {
                    Ok(meta) => {
                        record_remote_entry(&mut plan, &mut pending, src, dst, &meta, depth + 1)
                    }
                    Err(_) => continue, // dangling symlink
                }
            } else {
                record_remote_entry(&mut plan, &mut pending, src, dst, &meta, depth + 1);
            }
        }
    }

    Ok(plan)
}

fn record_remote_entry(
    plan: &mut DownloadPlan,
    pending: &mut Vec<(String, PathBuf, u32)>,
    src: String,
    dst: PathBuf,
    meta: &MetaData,
    depth: u32,
) {
    let attrs = RemoteAttrs::from_meta(meta);
    match meta.file_type() {
        Some(kind) if kind.is_dir() => {
            plan.dirs.push(RemoteDirJob {
                dst: dst.clone(),
                attrs,
            });
            pending.push((src, dst, depth));
        }
        Some(kind) if kind.is_file() => {
            let size = meta.len().unwrap_or(0);
            plan.total = plan.total.saturating_add(size);
            plan.files.push(RemoteFileJob {
                src,
                dst,
                size,
                attrs,
            });
        }
        // Sockets, fifos, and devices cannot travel over SFTP.
        _ => {}
    }
}

async fn download_file_sequential(
    sftp: &Arc<Sftp>,
    job: &RemoteFileJob,
    resuming: bool,
    reporter: &mut Reporter<impl FnMut(TransferUpdate)>,
) -> Result<(), TransferError> {
    let mut handle = sftp.open(&job.src).await.map_err(sftp_error)?;
    let mut local = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(!resuming)
        .open(&job.dst)
        .await
        .map_err(local_error)?;

    // Sequential downloads write contiguously, so the local length is exactly
    // what a previous attempt already fetched.
    let mut offset = if resuming {
        let len = local.metadata().await.map_err(local_error)?.len();
        if len > job.size {
            local.set_len(0).await.map_err(local_error)?;
            0
        } else {
            len
        }
    } else {
        0
    };
    if offset > 0 {
        local
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(local_error)?;
        reporter.add(offset);
    }

    loop {
        handle
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|err| TransferError::retryable(err.to_string()))?;
        let chunk = read_remote_chunk(&mut handle, DOWNLOAD_CHUNK).await?;
        let Some(chunk) = chunk else {
            break;
        };
        local.write_all(&chunk).await.map_err(local_error)?;
        offset += chunk.len() as u64;
        reporter.add(chunk.len() as u64);
    }
    local.flush().await.map_err(local_error)?;
    job.attrs.apply(&local.into_std().await);
    handle.close().await.map_err(sftp_error)?;
    Ok(())
}

#[cfg(unix)]
async fn download_file_striped(
    sftp: &Arc<Sftp>,
    job: &RemoteFileJob,
    stripes: &mut HashMap<usize, Arc<StripeProgress>>,
    file_idx: usize,
    reporter: &mut Reporter<impl FnMut(TransferUpdate)>,
) -> Result<(), TransferError> {
    use std::os::unix::fs::FileExt;

    let local = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false) // keep already-fetched chunks for resume
        .open(&job.dst)
        .map_err(local_error)?;
    local.set_len(job.size).map_err(local_error)?;
    let local = Arc::new(local);
    let size = job.size;

    let progress = Arc::clone(
        stripes
            .entry(file_idx)
            .or_insert_with(|| Arc::new(StripeProgress::new(DOWNLOAD_CHUNK))),
    );

    let mut tasks: JoinSet<Result<(), TransferError>> = JoinSet::new();
    for stripe in 0..DOWNLOAD_STRIPES as usize {
        let sftp = Arc::clone(sftp);
        let local = Arc::clone(&local);
        let counter = Arc::clone(&reporter.counter);
        let progress = Arc::clone(&progress);
        let src = job.src.clone();
        tasks.spawn(async move {
            let chunk_size = progress.chunk_size;
            let mut handle = sftp.open(&src).await.map_err(sftp_error)?;
            let mut chunk_index = progress.next[stripe].load(Ordering::Relaxed);
            'chunks: loop {
                let start = chunk_index.saturating_mul(chunk_size);
                if start >= size {
                    break;
                }
                let end = (start + chunk_size).min(size);
                let mut offset = start;
                while offset < end {
                    handle
                        .seek(std::io::SeekFrom::Start(offset))
                        .await
                        .map_err(|err| TransferError::retryable(err.to_string()))?;
                    let chunk = read_remote_chunk(&mut handle, end - offset).await?;
                    let Some(chunk) = chunk else {
                        break 'chunks; // remote file shrank mid-transfer
                    };
                    local.write_at(&chunk, offset).map_err(local_error)?;
                    offset += chunk.len() as u64;
                    counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                chunk_index += DOWNLOAD_STRIPES;
                progress.next[stripe].store(chunk_index, Ordering::Relaxed);
            }
            handle.close().await.map_err(sftp_error)?;
            Ok(())
        });
    }

    // Let every stripe settle rather than aborting on the first error: a
    // task cancelled between receiving a chunk and recording it would lose
    // that chunk's resume progress. On a dead connection the other stripes'
    // pending requests all fail promptly anyway.
    let mut first_err: Option<TransferError> = None;
    loop {
        tokio::select! {
            joined = tasks.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(err))) => first_err = first_err.or(Some(err)),
                Some(Err(err)) => {
                    first_err = first_err.or(Some(TransferError::retryable(format!(
                        "download task failed: {err}"
                    ))));
                }
            },
            _ = tokio::time::sleep(PROGRESS_INTERVAL) => reporter.tick(false),
        }
    }
    if let Some(err) = first_err {
        return Err(err);
    }

    job.attrs.apply(&local);
    Ok(())
}

#[cfg(not(unix))]
async fn download_file_striped(
    sftp: &Arc<Sftp>,
    job: &RemoteFileJob,
    _stripes: &mut HashMap<usize, Arc<StripeProgress>>,
    _file_idx: usize,
    reporter: &mut Reporter<impl FnMut(TransferUpdate)>,
) -> Result<(), TransferError> {
    download_file_sequential(sftp, job, false, reporter).await
}

/// Read up to `want` bytes at the handle's current offset. Returns `None` on
/// EOF; the returned buffer may be shorter than `want`.
async fn read_remote_chunk(
    handle: &mut openssh_sftp_client::file::File,
    want: u64,
) -> Result<Option<BytesMut>, TransferError> {
    let want: u32 = want.min(DOWNLOAD_CHUNK).try_into().expect("chunk fits u32");
    let buffer = BytesMut::with_capacity(want as usize);
    let chunk = handle.read(want, buffer).await.map_err(sftp_error)?;
    Ok(chunk.filter(|chunk| !chunk.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    /// The transfers run against a local OpenSSH `sftp-server` speaking the
    /// protocol on stdio, so the whole engine is exercised without sshd.
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

    type ProgressEntries = Vec<(u64, Option<u64>)>;

    #[derive(Clone, Default)]
    struct ProgressLog(Arc<Mutex<ProgressEntries>>);

    impl ProgressLog {
        fn recorder(&self) -> impl FnMut(TransferUpdate) + Send + use<> {
            let log = Arc::clone(&self.0);
            move |update| {
                if let TransferUpdate::Progress {
                    transferred_bytes,
                    total_bytes,
                } = update
                {
                    log.lock().unwrap().push((transferred_bytes, total_bytes));
                }
            }
        }

        fn entries(&self) -> ProgressEntries {
            self.0.lock().unwrap().clone()
        }
    }

    fn run(
        source: Pane,
        local_path: &Path,
        remote_path: &str,
        names: &[&str],
        progress: &ProgressLog,
    ) -> Result<(), String> {
        let server = sftp_server_path().expect("sftp-server available");
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        run_transfer_with(
            || Ok(Command::new(server)),
            source,
            local_path,
            remote_path,
            &names,
            progress.recorder(),
        )
    }

    fn assert_same_bytes(actual: &[u8], expected: &[u8], what: &str) {
        assert_eq!(actual.len(), expected.len(), "{what}: length mismatch");
        if let Some(pos) = actual.iter().zip(expected).position(|(a, b)| a != b) {
            panic!(
                "{what}: first mismatch at byte {pos}: {} vs {}",
                actual[pos], expected[pos]
            );
        }
    }

    fn assert_progress_reaches(progress: &ProgressLog, expected_total: u64) {
        let entries = progress.entries();
        let transferred: Vec<u64> = entries.iter().map(|(bytes, _)| *bytes).collect();
        assert!(
            transferred.windows(2).all(|pair| pair[0] <= pair[1]),
            "progress should be monotonic: {transferred:?}"
        );
        let last = entries.last().expect("progress reported");
        assert_eq!(last.0, expected_total);
        assert_eq!(last.1, Some(expected_total));
    }

    #[test]
    fn uploads_files_and_directories() {
        if sftp_server_path().is_none() {
            eprintln!("skipping: no local sftp-server binary");
            return;
        }
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("top.bin"), b"abc").unwrap();
        std::fs::create_dir(local.path().join("nested")).unwrap();
        std::fs::create_dir(local.path().join("nested/empty")).unwrap();
        std::fs::write(local.path().join("nested/child.bin"), vec![7u8; 5000]).unwrap();
        let old_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(local.path().join("top.bin"))
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                local.path().join("top.bin"),
                std::fs::Permissions::from_mode(0o751),
            )
            .unwrap();
        }

        let progress = ProgressLog::default();
        run(
            Pane::Local,
            local.path(),
            remote.path().to_str().unwrap(),
            &["top.bin", "nested"],
            &progress,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(remote.path().join("top.bin")).unwrap(),
            b"abc"
        );
        assert_eq!(
            std::fs::read(remote.path().join("nested/child.bin")).unwrap(),
            vec![7u8; 5000]
        );
        assert!(remote.path().join("nested/empty").is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(remote.path().join("top.bin"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o7777, 0o751, "upload should preserve permissions");
        }
        let uploaded_mtime = std::fs::metadata(remote.path().join("top.bin"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(uploaded_mtime, old_mtime, "upload should preserve mtime");
        assert_progress_reaches(&progress, 5003);
    }

    #[test]
    fn downloads_large_files_striped() {
        if sftp_server_path().is_none() {
            eprintln!("skipping: no local sftp-server binary");
            return;
        }
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        // Non-repeating pattern across several stripes catches any chunk
        // ordering or offset mistakes.
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(remote.path().join("big.bin"), &big).unwrap();
        std::fs::create_dir(remote.path().join("dir")).unwrap();
        std::fs::write(remote.path().join("dir/small.bin"), b"hello").unwrap();

        let progress = ProgressLog::default();
        run(
            Pane::Remote,
            local.path(),
            remote.path().to_str().unwrap(),
            &["big.bin", "dir"],
            &progress,
        )
        .unwrap();

        assert_eq!(std::fs::read(local.path().join("big.bin")).unwrap(), big);
        assert_eq!(
            std::fs::read(local.path().join("dir/small.bin")).unwrap(),
            b"hello"
        );
        assert_progress_reaches(&progress, big.len() as u64 + 5);
    }

    #[test]
    fn upload_overwrites_existing_remote_file() {
        if sftp_server_path().is_none() {
            eprintln!("skipping: no local sftp-server binary");
            return;
        }
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("data.bin"), b"new").unwrap();
        std::fs::write(remote.path().join("data.bin"), b"old longer contents").unwrap();

        run(
            Pane::Local,
            local.path(),
            remote.path().to_str().unwrap(),
            &["data.bin"],
            &ProgressLog::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read(remote.path().join("data.bin")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn download_reports_missing_remote_entry() {
        if sftp_server_path().is_none() {
            eprintln!("skipping: no local sftp-server binary");
            return;
        }
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();

        let err = run(
            Pane::Remote,
            local.path(),
            remote.path().to_str().unwrap(),
            &["missing.bin"],
            &ProgressLog::default(),
        )
        .unwrap_err();

        assert!(err.contains("missing.bin"), "unexpected error: {err}");
    }

    /// A server wrapper whose connection dies after passing `limit` bytes in
    /// the given direction, to simulate a mid-transfer connection drop.
    /// `dd bs=1` is the one portable byte limiter that forwards immediately;
    /// BSD `head` (and `dd` with larger blocks) buffer partial blocks, which
    /// would stall the SFTP handshake instead of cutting mid-stream.
    fn truncated_server(server: &str, direction: Pane, limit: u64) -> Command {
        let script = match direction {
            // Cut the client->server stream: uploads die partway.
            Pane::Local => format!("dd bs=1 count={limit} 2>/dev/null | {server}"),
            // Cut the server->client stream: downloads die partway.
            Pane::Remote => format!("{server} | dd bs=1 count={limit} 2>/dev/null"),
        };
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn upload_resumes_after_connection_drop() {
        let Some(server) = sftp_server_path() else {
            eprintln!("skipping: no local sftp-server binary");
            return;
        };
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..3_000_000u32).map(|i| (i % 249) as u8).collect();
        std::fs::write(local.path().join("big.bin"), &content).unwrap();

        let attempts = Arc::new(AtomicU32::new(0));
        let partial_at_retry = Arc::new(AtomicU64::new(0));
        let remote_file = remote.path().join("big.bin");
        let result = run_transfer_with(
            {
                let attempts = Arc::clone(&attempts);
                let partial_at_retry = Arc::clone(&partial_at_retry);
                let remote_file = remote_file.clone();
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        return Ok(truncated_server(server, Pane::Local, 400_000));
                    }
                    // Prove the retry resumes rather than restarting: corrupt
                    // the first byte of the partial upload. A resume keeps the
                    // existing prefix (like `reput`), so the flip survives.
                    let mut partial = std::fs::read(&remote_file).unwrap();
                    assert!(!partial.is_empty(), "expected a partial upload");
                    partial_at_retry.store(partial.len() as u64, Ordering::Relaxed);
                    partial[0] ^= 0xff;
                    std::fs::write(&remote_file, partial).unwrap();
                    Ok(Command::new(server))
                }
            },
            Pane::Local,
            local.path(),
            remote.path().to_str().unwrap(),
            &[String::from("big.bin")],
            ProgressLog::default().recorder(),
        );

        result.unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 2, "one retry expected");
        let partial = partial_at_retry.load(Ordering::Relaxed);
        assert!(
            partial > 0 && partial < content.len() as u64,
            "first attempt should stop partway, got {partial}"
        );
        let mut expected = content.clone();
        expected[0] ^= 0xff;
        let uploaded = std::fs::read(&remote_file).unwrap();
        assert_same_bytes(
            &uploaded,
            &expected,
            "resume should keep the existing prefix",
        );
    }

    #[test]
    fn download_resumes_after_connection_drop() {
        let Some(server) = sftp_server_path() else {
            eprintln!("skipping: no local sftp-server binary");
            return;
        };
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..3_000_000u32).map(|i| (i % 247) as u8).collect();
        std::fs::write(remote.path().join("big.bin"), &content).unwrap();

        let attempts = Arc::new(AtomicU32::new(0));
        let local_file = local.path().join("big.bin");
        let result = run_transfer_with(
            {
                let attempts = Arc::clone(&attempts);
                let local_file = local_file.clone();
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        // ~600KB of responses: the first stripe chunk (256KB)
                        // completes before the cut.
                        return Ok(truncated_server(server, Pane::Remote, 600_000));
                    }
                    // Prove the retry skips completed chunks: corrupt the
                    // first byte locally; a resume never re-reads chunk 0.
                    let mut partial = std::fs::read(&local_file).unwrap();
                    partial[0] ^= 0xff;
                    std::fs::write(&local_file, partial).unwrap();
                    Ok(Command::new(server))
                }
            },
            Pane::Remote,
            local.path(),
            remote.path().to_str().unwrap(),
            &[String::from("big.bin")],
            ProgressLog::default().recorder(),
        );

        result.unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 2, "one retry expected");
        let mut expected = content.clone();
        expected[0] ^= 0xff;
        let downloaded = std::fs::read(&local_file).unwrap();
        assert_same_bytes(
            &downloaded,
            &expected,
            "resume should keep already-fetched chunks",
        );
    }

    #[test]
    fn gives_up_after_repeated_failures_without_progress() {
        let local = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("data.bin"), b"payload").unwrap();

        let attempts = Arc::new(AtomicU32::new(0));
        let started = Instant::now();
        let err = run_transfer_with(
            {
                let attempts = Arc::clone(&attempts);
                move || {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    // A server that exits immediately: every attempt fails
                    // before any progress is made.
                    Ok(Command::new("true"))
                }
            },
            Pane::Local,
            local.path(),
            "/nonexistent-remote",
            &[String::from("data.bin")],
            ProgressLog::default().recorder(),
        )
        .unwrap_err();

        assert_eq!(attempts.load(Ordering::Relaxed), MAX_STALLED_ATTEMPTS);
        assert!(!err.is_empty());
        assert!(
            started.elapsed() >= Duration::from_millis(1000),
            "retries should back off"
        );
    }
}
