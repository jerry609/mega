use std::{
    collections::HashMap,
    error::Error,
    io::{self, BufReader},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::anyhow;
use api_model::buck2::{
    status::Status,
    types::{ProjectRelativePath, TaskPhase},
    ws::{WSBuildContext, WSMessage, WSTargetBuildStatusEvent},
};
use common::config::BuildConfig;
use once_cell::sync::Lazy;
use td_util::{command::spawn, file_io::file_writer, file_tail::tail_compressed_buck2_events};
use td_util_buck::{
    cells::CellInfo,
    run::{Buck2, targets_arguments},
    target_status::{BuildState, EVENT_LOG_FILE, Event, LogicalActionId, TargetBuildStatusUpdate},
    targets::Targets,
    types::TargetLabel,
};
use tokio::{
    io::AsyncBufReadExt,
    process::Command,
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
    time::{Duration, Instant, interval},
};
use tokio_util::sync::CancellationToken;

// Import complete Error trait for better error handling
use crate::repo::changes::Changes;
use crate::repo::diff;

const MAX_BATCH_SIZE: usize = 100;
const FLUSH_INTERVAL_MS: u64 = 100;
const MOUNT_READY_TIMEOUT_SECS: u64 = 15;
const MOUNT_READY_POLL_INTERVAL_MS: u64 = 200;
const MOUNT_READY_SINGLE_PROBE_TIMEOUT_SECS: u64 = 2;
const LOCAL_BUCK_OUT_ROOT: &str = "/tmp/orion-buck-out";

#[allow(dead_code)]
static PROJECT_ROOT: Lazy<String> =
    Lazy::new(|| std::env::var("BUCK_PROJECT_ROOT").expect("BUCK_PROJECT_ROOT must be set"));

const DEFAULT_PREHEAT_SHALLOW_DEPTH: usize = 3;
static BUILD_CONFIG: Lazy<Option<BuildConfig>> = Lazy::new(load_build_config);

/// Mount an Antares overlay filesystem for a build job.
///
/// Creates a new Antares overlay mount using scorpiofs. The underlying Dicfuse
/// layer provides read-only access to the repository, while the overlay allows
/// copy-on-write modifications during the build.
///
/// # Arguments
/// * `job_id` - Unique identifier for this build job
/// * `cl` - Optional changelist layer name
///
/// # Returns
/// Returns a tuple `(mountpoint, mount_id)` on success.
pub async fn mount_antares_fs(
    job_id: &str,
    cl: Option<&str>,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    tracing::debug!(
        "Preparing to mount Antares FS: job_id={}, cl={:?}",
        job_id,
        cl
    );

    let config = crate::antares::mount_job(job_id, cl).await?;

    let mountpoint = config.mountpoint.to_string_lossy().to_string();
    let mount_id = config.job_id.clone();

    tracing::info!(
        "Antares mount created successfully: mountpoint={}, mount_id={}",
        mountpoint,
        mount_id
    );

    Ok((mountpoint, mount_id))
}

/// Unmount an Antares overlay filesystem.
///
/// # Arguments
/// * `mount_id` - The job/mount ID to unmount
///
/// # Returns
/// * `Ok(true)` - Unmount succeeded
/// * `Err` - Unmount failed
pub async fn unmount_antares_fs(mount_id: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
    tracing::info!("Starting unmount for mount_id: {}", mount_id);

    crate::antares::unmount_job(mount_id).await?;

    tracing::info!("Unmount succeeded for mount_id: {}", mount_id);
    Ok(true)
}

/// Pre-warm FUSE metadata by walking the directory tree.
///
/// After the Antares `wait_for_mount_ready` integration, the heavy deep preload
/// is performed by the scorpio daemon itself. This function serves as a secondary
/// fallback to catch any directories that might have been missed or expired.
///
/// We use a shallow Rust-native walk (instead of `ls -lR`) for better control
/// and error resilience.
fn preheat(repo_path: &Path) -> anyhow::Result<()> {
    tracing::info!(repo = ?repo_path, "preheat: starting lightweight metadata warmup");
    let start = std::time::Instant::now();
    let depth = preheat_shallow_depth();
    preheat_shallow(repo_path, depth)?;
    tracing::info!(
        repo = ?repo_path,
        elapsed_ms = start.elapsed().as_millis(),
        depth = depth,
        "preheat: completed"
    );
    Ok(())
}

/// Preheat a shallow directory tree to reduce cold-start metadata misses.
fn preheat_shallow(repo_path: &Path, max_depth: usize) -> anyhow::Result<()> {
    if max_depth == 0 {
        return Ok(());
    }

    let mut stack = vec![(repo_path.to_path_buf(), 0usize)];
    while let Some((path, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!("Preheat shallow read_dir failed for {path:?}: {err}");
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::warn!("Preheat shallow entry error under {path:?}: {err}");
                    continue;
                }
            };

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    tracing::warn!(
                        "Preheat shallow file_type failed for {:?}: {err}",
                        entry.path()
                    );
                    continue;
                }
            };

            // Touch metadata to warm FUSE cache.
            let _ = entry.metadata();

            if file_type.is_dir() && depth < max_depth {
                stack.push((entry.path(), depth + 1));
            }
        }
    }

    Ok(())
}

fn preheat_shallow_depth() -> usize {
    if let Some(depth) = parse_env_usize("ORION_PREHEAT_SHALLOW_DEPTH") {
        return depth;
    }

    if let Some(depth) = parse_env_usize("MEGA_BUILD__ORION_PREHEAT_SHALLOW_DEPTH") {
        return depth;
    }

    build_config()
        .map(|config| config.orion_preheat_shallow_depth)
        .unwrap_or(DEFAULT_PREHEAT_SHALLOW_DEPTH)
}

fn parse_env_usize(key: &str) -> Option<usize> {
    match std::env::var(key) {
        Ok(value) => match value.parse::<usize>() {
            Ok(depth) => Some(depth),
            Err(_) => {
                tracing::warn!("Invalid {key}={value:?}, ignoring.");
                None
            }
        },
        Err(_) => None,
    }
}

fn build_config() -> Option<&'static BuildConfig> {
    BUILD_CONFIG.as_ref()
}

fn load_build_config() -> Option<BuildConfig> {
    let path = resolve_config_path()?;
    let path_str = path.to_str()?;
    match common::config::Config::new(path_str) {
        Ok(config) => Some(config.build),
        Err(err) => {
            tracing::warn!("Failed to load MEGA config from {path:?}: {err}");
            None
        }
    }
}

fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("MEGA_CONFIG") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
        tracing::warn!("MEGA_CONFIG points to missing file: {path:?}");
    }

    if let Ok(cwd) = std::env::current_dir() {
        let path = cwd.join("config/config.toml");
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(base_dir) = std::env::var("MEGA_BASE_DIR") {
        let path = PathBuf::from(base_dir).join("etc/config.toml");
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Derive a stable per-mount buck2 isolation directory name.
///
/// # Why `--isolation-dir` is required
///
/// Although we always mount the **same monorepo** (`path = "/"`), every call
/// to `mount_antares_fs()` goes through `POST /antares/mounts` which creates
/// a **new UUID** and therefore a **new mountpoint path** each time:
///
/// ```text
///   build task A  →  mount_antares_fs(job="A-1", path="/", cl=None)
///                     → mountpoint = /var/lib/antares/mounts/<uuid-1>
///
///   build task A  →  mount_antares_fs(job="A-1", path="/", cl="CL-42")
///                     → mountpoint = /var/lib/antares/mounts/<uuid-2>
///
///   build task B  →  mount_antares_fs(job="B-1", path="/", cl=None)
///                     → mountpoint = /var/lib/antares/mounts/<uuid-3>
/// ```
///
/// Without `--isolation-dir`, Buck2 uses a **single default daemon** per
/// `<project_root>`.  Because the project root changes with every mount UUID,
/// this usually just causes redundant daemon restarts.  But if two concurrent
/// builds happen to share the same `project_root` (e.g., via retry in
/// `MAX_TARGETS_ATTEMPTS`), the second buck2 invocation would talk to the
/// first daemon whose internal paths point at a **stale** mountpoint — leading
/// to `ESTALE` / `ENOENT` cascades.
///
/// By deriving `--isolation-dir` from `SHA256(repo_path)`, we get:
/// - **Same mount path → same daemon** (avoids daemon startup cost on retry)
/// - **Different mount paths → different daemons** (avoids cross-contamination)
///
/// # Format
///
/// Buck2 `--isolation-dir` expects a plain directory **name** (no path
/// separators).  Buck2 itself stores daemon state under
/// `<project_root>/.buck2/<isolation_dir>/`, so we only need to return a
/// unique name – not a full path.
fn buck2_isolation_dir(repo_path: &Path) -> anyhow::Result<String> {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        repo_path.to_string_lossy().as_bytes(),
    );
    let suffix = &hex::encode(digest.as_ref())[..16];
    Ok(format!("buck2-isolation-{suffix}"))
}

/// Best-effort cleanup for per-mount buck2 daemons.
///
/// With unique mountpoints each build gets a fresh `--isolation-dir`, which means
/// buck2 may keep many idle daemons alive. Over time that can exhaust host limits
/// (e.g. inotify/file locks) and make subsequent daemon startups fail.
fn cleanup_buck2_daemon(repo_path: &Path) {
    let Ok(isolation_dir) = buck2_isolation_dir(repo_path) else {
        return;
    };

    let status = std::process::Command::new("buck2")
        .args(["--isolation-dir", &isolation_dir, "kill"])
        .current_dir(repo_path)
        .status();

    match status {
        Ok(exit) if exit.success() => {
            tracing::debug!(
                "Cleaned up buck2 daemon for repo {:?} (isolation_dir={})",
                repo_path,
                isolation_dir
            );
        }
        Ok(exit) => {
            tracing::debug!(
                "buck2 kill returned non-zero for repo {:?} (isolation_dir={}, status={})",
                repo_path,
                isolation_dir,
                exit
            );
        }
        Err(err) => {
            tracing::debug!(
                "buck2 kill failed for repo {:?} (isolation_dir={}): {}",
                repo_path,
                isolation_dir,
                err
            );
        }
    }
}

fn local_buck_out_dir(repo_path: &Path) -> anyhow::Result<PathBuf> {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        repo_path.to_string_lossy().as_bytes(),
    );
    let suffix = &hex::encode(digest.as_ref())[..16];
    Ok(PathBuf::from(LOCAL_BUCK_OUT_ROOT).join(format!("buck-out-{suffix}")))
}

fn ensure_local_buck_out(repo_path: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::symlink;

    let mounted_buck_out = repo_path.join("buck-out");
    let local_buck_out = local_buck_out_dir(repo_path)?;

    std::fs::create_dir_all(&local_buck_out)?;

    match std::fs::symlink_metadata(&mounted_buck_out) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let current_target = std::fs::read_link(&mounted_buck_out)?;
            if current_target == local_buck_out {
                return Ok(local_buck_out);
            }
            return Err(anyhow!(
                "Mounted repo {:?} already has buck-out symlink {:?}, expected {:?}",
                repo_path,
                current_target,
                local_buck_out,
            ));
        }
        Ok(_) => {
            return Err(anyhow!(
                "Mounted repo {:?} already has non-symlink buck-out at {:?}",
                repo_path,
                mounted_buck_out,
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    symlink(&local_buck_out, &mounted_buck_out)?;
    tracing::info!(
        repo = ?repo_path,
        local_buck_out = ?local_buck_out,
        "Prepared local buck-out symlink for mounted repo"
    );
    Ok(local_buck_out)
}

fn cleanup_local_buck_out(repo_path: &Path) {
    let Ok(local_buck_out) = local_buck_out_dir(repo_path) else {
        return;
    };

    let mounted_buck_out = repo_path.join("buck-out");
    if let Ok(metadata) = std::fs::symlink_metadata(&mounted_buck_out)
        && metadata.file_type().is_symlink()
    {
        if let Err(err) = std::fs::remove_file(&mounted_buck_out) {
            tracing::debug!(
                repo = ?repo_path,
                path = ?mounted_buck_out,
                error = %err,
                "Failed to remove mounted buck-out symlink"
            );
        }
    }

    if let Err(err) = std::fs::remove_dir_all(&local_buck_out)
        && err.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!(
            repo = ?repo_path,
            path = ?local_buck_out,
            error = %err,
            "Failed to remove local buck-out directory"
        );
    }
}

fn is_retryable_mount_io_error(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::TimedOut || matches!(err.raw_os_error(), Some(107 | 116 | 5 | 2))
}

fn is_retryable_target_discovery_error(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    [
        "Transport endpoint is not connected",
        "os error 107",
        "Stale file handle",
        "os error 116",
        "Failed to connect to buck daemon",
        "No such file or directory",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn probe_repo_mount(repo_path: &Path) -> io::Result<()> {
    let metadata = std::fs::metadata(repo_path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{:?} is not a directory", repo_path),
        ));
    }

    let mut entries = std::fs::read_dir(repo_path)?;
    if let Some(entry) = entries.next() {
        let entry = entry?;
        let _ = entry.file_type()?;
    }

    let mut child = std::process::Command::new("true")
        .current_dir(repo_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = std::time::Instant::now();

    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(io::Error::other(format!(
                "current_dir probe exited with status {status}"
            )));
        }

        if start.elapsed() >= Duration::from_secs(MOUNT_READY_SINGLE_PROBE_TIMEOUT_SECS) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "current_dir probe timed out after {}s for {:?}",
                    MOUNT_READY_SINGLE_PROBE_TIMEOUT_SECS, repo_path
                ),
            ));
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn wait_for_repo_mount_ready_with_retry(
    repo_path: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_error: Option<io::Error> = None;

    loop {
        let repo_path_buf = repo_path.to_path_buf();
        let probe_result = tokio::time::timeout(
            Duration::from_secs(MOUNT_READY_SINGLE_PROBE_TIMEOUT_SECS),
            tokio::task::spawn_blocking(move || probe_repo_mount(&repo_path_buf)),
        )
        .await;

        match probe_result {
            Ok(Ok(Ok(()))) => {
                tracing::debug!(repo = ?repo_path, "Mounted repo path is ready");
                return Ok(());
            }
            Ok(Ok(Err(err))) if is_retryable_mount_io_error(&err) => {
                let timed_out = Instant::now() >= deadline;
                let raw_code = err.raw_os_error();
                let should_log = last_error
                    .as_ref()
                    .map(|previous| previous.raw_os_error() != raw_code)
                    .unwrap_or(true);

                if timed_out {
                    return Err(anyhow!(
                        "Timed out waiting for mounted repo path {:?} to become ready: {}",
                        repo_path,
                        err
                    ));
                }

                if should_log {
                    tracing::warn!(
                        repo = ?repo_path,
                        error = %err,
                        "Mounted repo path is not ready yet; retrying"
                    );
                }

                last_error = Some(err);
                tokio::time::sleep(poll_interval).await;
            }
            Ok(Ok(Err(err))) => {
                return Err(anyhow!(
                    "Mounted repo path {:?} is not accessible: {}",
                    repo_path,
                    err
                ));
            }
            Ok(Err(err)) => {
                return Err(anyhow!(
                    "Mounted repo path probe panicked for {:?}: {}",
                    repo_path,
                    err
                ));
            }
            Err(_) => {
                if Instant::now() >= deadline {
                    return Err(anyhow!(
                        "Timed out waiting for mounted repo path {:?}: probe exceeded {}s repeatedly",
                        repo_path,
                        MOUNT_READY_SINGLE_PROBE_TIMEOUT_SECS
                    ));
                }

                tracing::warn!(
                    repo = ?repo_path,
                    probe_timeout_secs = MOUNT_READY_SINGLE_PROBE_TIMEOUT_SECS,
                    "Mounted repo probe timed out; retrying"
                );
                tokio::time::sleep(poll_interval).await;
            }
        }
    }
}

async fn wait_for_repo_mount_ready(repo_path: &Path) -> anyhow::Result<()> {
    wait_for_repo_mount_ready_with_retry(
        repo_path,
        Duration::from_secs(MOUNT_READY_TIMEOUT_SECS),
        Duration::from_millis(MOUNT_READY_POLL_INTERVAL_MS),
    )
    .await
}

/// Get target of a specific repo under tmp directory.
fn get_repo_targets(file_name: &str, repo_path: &Path) -> anyhow::Result<Targets> {
    const MAX_ATTEMPTS: usize = 2;
    let jsonl_path = PathBuf::from(repo_path).join(file_name);
    let isolation_dir = buck2_isolation_dir(repo_path)?;

    preheat(repo_path)?;

    for attempt in 1..=MAX_ATTEMPTS {
        tracing::debug!("Get targets for repo {repo_path:?} (attempt {attempt}/{MAX_ATTEMPTS})");
        let mut command = std::process::Command::new("buck2");
        command
            .args(["--isolation-dir", &isolation_dir])
            .args(targets_arguments());
        command.current_dir(repo_path);
        let (mut child, stdout) = spawn(command)?;
        let mut writer = file_writer(&jsonl_path)?;
        std::io::copy(&mut BufReader::new(stdout), &mut writer)
            .map_err(|err| anyhow!("Failed to copy output to stdout: {}", err))?;
        writer
            .flush()
            .map_err(|err| anyhow!("Failed to flush writer: {}", err))?;
        let status = child.wait()?;
        if status.success() {
            let targets = Targets::from_file(&jsonl_path);
            cleanup_buck2_daemon(repo_path);
            return targets;
        }

        cleanup_buck2_daemon(repo_path);
        tracing::warn!(
            "buck2 targets failed with status {} for repo {:?}",
            status,
            repo_path
        );
        if attempt < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    Err(anyhow!(
        "buck2 targets failed after {MAX_ATTEMPTS} attempts for repo {:?}",
        repo_path
    ))
}

/// Run buck2-change-detector to get targets to build.
///
/// # Note
/// `mount_point` must be a mounted repository or CL path.
async fn get_build_targets(
    old_repo_mount_point: &str,
    mount_point: &str,
    mega_changes: Vec<Status<ProjectRelativePath>>,
) -> anyhow::Result<Vec<TargetLabel>> {
    tracing::info!("Get cells at {:?}", mount_point);
    let mount_path = PathBuf::from(mount_point);
    let old_repo = PathBuf::from(old_repo_mount_point);
    tracing::debug!("Analyzing changes {mega_changes:?}");

    preheat_shallow(&mount_path, preheat_shallow_depth())?;
    let mut buck2 = Buck2::with_root("buck2".to_string(), mount_path.clone());
    buck2.set_isolation_dir(buck2_isolation_dir(&mount_path)?);
    let mut cells = CellInfo::parse(
        &buck2
            .cells()
            .map_err(|err| anyhow!("Fail to get cells: {}", err))?,
    )?;

    tracing::debug!("Get config");
    cells.parse_config_data(
        &buck2
            .audit_config()
            .map_err(|err| anyhow!("Fail to get config: {}", err))?,
    )?;

    let base = get_repo_targets("base.jsonl", &old_repo)?;
    let changes = Changes::new(&cells, mega_changes)?;
    tracing::debug!("Changes {changes:?}");
    let diff = get_repo_targets("diff.jsonl", &mount_path)?;

    tracing::debug!("Base targets number: {}", base.len_targets_upperbound());
    tracing::debug!("Diff targets number: {}", diff.len_targets_upperbound());

    let immediate = diff::immediate_target_changes(&base, &diff, &changes, false);
    let recursive = diff::recursive_target_changes(&diff, &changes, &immediate, None, |_| true);

    Ok(recursive
        .into_iter()
        .flatten()
        .map(|(target, _)| target.label())
        .collect())
}

#[derive(Debug)]
pub struct BuildStatusTracker {
    pub cancellation: CancellationToken,
    pub tail_handle: JoinHandle<()>,
    pub process_handle: JoinHandle<()>,
}

/// Start the build status tracker.
/// Returns a `BuildStatusTracker` for lifecycle management.
pub fn start_build_status_tracker(
    project_root: &Path,
    sender: UnboundedSender<WSMessage>,
    cl_id: &str,
    task_id: &str,
) -> BuildStatusTracker {
    let event_jsonl_path = project_root.join(EVENT_LOG_FILE);
    tracing::info!("Track target build status at {:?}", event_jsonl_path);

    let (tx, rx) = mpsc::channel::<String>(4096);
    let cancellation = CancellationToken::new();

    // Spawn tail task: reads the event log file and sends each line into channel
    let tail_handle = {
        let event_log_path = event_jsonl_path.clone();
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let poll_interval = Duration::from_millis(200);
            if let Err(e) =
                tail_compressed_buck2_events(event_log_path, tx_clone, poll_interval).await
            {
                tracing::error!("Failed to tail buck2 events: {:?}", e);
            }
        })
    };

    // Spawn processing task: handles events, updates state, flushes WebSocket
    let process_handle = {
        let sender_clone = sender.clone();
        let cancellation_clone = cancellation.clone();
        tokio::spawn(run_processing_loop(
            rx,
            sender_clone,
            cancellation_clone,
            cl_id.to_owned(),
            task_id.to_owned(),
        ))
    };

    BuildStatusTracker {
        cancellation,
        tail_handle,
        process_handle,
    }
}

/// Event processing loop.
/// Reads event lines, updates BuildState, and flushes WebSocket in batches or intervals.
async fn run_processing_loop(
    mut rx: mpsc::Receiver<String>,
    sender: UnboundedSender<WSMessage>,
    cancellation: CancellationToken,
    cl_id: String,
    task_id: String,
) {
    let mut build_state = BuildState::default();
    let mut buffer: HashMap<LogicalActionId, TargetBuildStatusUpdate> = HashMap::with_capacity(256);
    let mut flush_interval = interval(Duration::from_millis(FLUSH_INTERVAL_MS));

    loop {
        tokio::select! {
            Some(line) = rx.recv() => {
                match serde_json::from_str::<Event>(&line) {
                    Ok(event) => {
                        if let Some(update) = build_state.handle_event(&event) {
                            buffer.insert(update.action_id.clone(), update);

                             if buffer.len() >= MAX_BATCH_SIZE
                                && !flush_buffer(&sender, &mut buffer, &cl_id, &task_id).await {
                                    break;
                                }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse event line: {:?}", e);
                    }
                }
            }

            _ = flush_interval.tick() => {
                if !buffer.is_empty()
                   && !flush_buffer(&sender, &mut buffer, &cl_id, &task_id).await {
                        break;
                    }
            }

            _ = cancellation.cancelled() => {
                tracing::info!("Cancellation received, stopping processing loop");
                break;
            }
        }
    }

    // Flush remaining updates
    if !buffer.is_empty() {
        let _ = flush_buffer(&sender, &mut buffer, &cl_id, &task_id).await;
    }

    tracing::info!("Build status processing loop stopped");
}

/// Flush the buffer of updates to the WebSocket.
/// Returns false if the WebSocket is closed.
async fn flush_buffer(
    sender: &UnboundedSender<WSMessage>,
    buffer: &mut HashMap<LogicalActionId, TargetBuildStatusUpdate>,
    cl_id: &str,
    task_id: &str,
) -> bool {
    if buffer.is_empty() {
        tracing::trace!(task_id, cl_id, "Buffer empty, skipping flush");
        return true;
    }

    let update_count = buffer.len();

    tracing::debug!(
        task_id,
        cl_id,
        update_count,
        "Flushing target build status updates"
    );

    let context = WSBuildContext {
        task_id: task_id.to_string(),
        cl_id: cl_id.to_string(),
    };

    let events: Vec<WSTargetBuildStatusEvent> = buffer
        .drain()
        .map(|(_, update)| WSTargetBuildStatusEvent {
            context: context.clone(),
            target: update.into(),
        })
        .collect();

    let message = WSMessage::TargetBuildStatusBatch { events };

    match sender.send(message) {
        Ok(_) => {
            tracing::trace!(
                task_id,
                cl_id,
                update_count,
                "Successfully flushed target build status updates"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                task_id,
                cl_id,
                update_count,
                error = %e,
                "Failed to send target build status batch"
            );
            false
        }
    }
}

/// RAII guard for automatically unmounting Antares filesystem when dropped
struct MountGuard {
    mount_id: String,
    task_id: String,
    unmounted: AtomicBool,
}

impl MountGuard {
    fn new(mount_id: String, task_id: String) -> Self {
        Self {
            mount_id,
            task_id,
            unmounted: AtomicBool::new(false),
        }
    }

    async fn unmount(&self) {
        if self.unmounted.swap(true, Ordering::AcqRel) {
            return;
        }
        match unmount_antares_fs(&self.mount_id).await {
            Ok(_) => tracing::info!("[Task {}] Filesystem unmounted successfully.", self.task_id),
            Err(e) => {
                tracing::error!(
                    "[Task {}] Failed to unmount filesystem: {}",
                    self.task_id,
                    e
                )
            }
        }
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if self.unmounted.load(Ordering::Acquire) {
            return;
        }
        let mount_id = self.mount_id.clone();
        let task_id: String = self.task_id.clone();
        tokio::spawn(async move {
            match unmount_antares_fs(&mount_id).await {
                Ok(_) => tracing::info!("[Task {}] Filesystem unmounted successfully.", task_id),
                Err(e) => tracing::error!("[Task {}] Failed to unmount filesystem: {}", task_id, e),
            }
        });
    }
}

/// Executes buck build with filesystem mounting and output streaming.
///
/// Process flow:
/// 1. Mount repository filesystem via remote API
/// 2. Execute buck build command with specified target and arguments  
/// 3. Stream build output in real-time via WebSocket
/// 4. Return final build status
///
/// # Arguments
/// * `id` - Build task identifier for logging and tracking
/// * `mount_path` - Monorepo path to mount (Buck2 project root or subdirectory)
/// * `target` - Buck build target specification  
/// * `args` - Additional command-line arguments for buck
/// * `cl` - Change List context identifier
/// * `sender` - WebSocket channel for streaming build output
/// * `changes` - Commit's file change information
///
/// # Returns
/// Process exit status indicating build success or failure
pub async fn build(
    id: String,
    repo: String,
    cl: String,
    sender: UnboundedSender<WSMessage>,
    changes: Vec<Status<ProjectRelativePath>>,
) -> Result<ExitStatus, Box<dyn Error + Send + Sync>> {
    tracing::info!("[Task {}] Building in repo {}", id, repo);

    let task_id = id.trim();
    // Handle empty cl string as None to mount the base repo without a CL layer.
    let cl_trimmed = cl.trim();
    let cl_arg = (!cl_trimmed.is_empty()).then_some(cl_trimmed);

    // `repo_prefix` is the relative path from monorepo root to the buck2 project.
    // e.g., repo="/project/git-internal/git-internal" → repo_prefix="project/git-internal/git-internal"
    let repo_prefix = repo.strip_prefix('/').unwrap_or(&repo);

    // Changes are already relative to the sub-project (buck2 project root).
    // Do NOT prefix them with repo_prefix — buck2 runs from the sub-project dir.

    const MAX_TARGETS_ATTEMPTS: usize = 3;
    let mut mount_point = None;
    let mut mount_guard = None;
    let mut mount_guard_old_repo = None;
    let mut old_project_root_saved = None;
    let mut targets: Vec<TargetLabel> = Vec::new();
    let mut last_targets_error: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_TARGETS_ATTEMPTS {
        // Mount TWO independent views of the same monorepo:
        //
        //   old_repo  — base revision (no CL), used as the "before" snapshot
        //   new_repo  — base + CL layer,       used as the "after"  snapshot
        //
        // Both mount the same monorepo root (`path = "/"`), but each call to
        // `mount_antares_fs()` creates a **new UUID** on the Antares side, so
        // the mountpoints are different (e.g. `/var/lib/antares/mounts/<uuid>`).
        // This is why `--isolation-dir` (derived from the mountpoint path) is
        // necessary — it prevents Buck2 daemons from cross-contaminating
        // between the two mounts.  See `buck2_isolation_dir()` for details.
        let id_for_old_repo = format!("{id}-old-{attempt}");
        let (old_repo_mount_point, mount_id_old_repo) =
            mount_antares_fs(&id_for_old_repo, None).await?;
        let guard_old_repo = MountGuard::new(mount_id_old_repo.clone(), id_for_old_repo);

        let id_for_repo = format!("{id}-{attempt}");
        let (repo_mount_point, mount_id) = mount_antares_fs(&id_for_repo, cl_arg).await?;
        let guard = MountGuard::new(mount_id.clone(), id_for_repo);

        tracing::info!(
            "[Task {}] Filesystem mounted successfully (attempt {}/{}).",
            id,
            attempt,
            MAX_TARGETS_ATTEMPTS
        );

        // Resolve the sub-project paths within each mount for buck2.
        let old_project_root = PathBuf::from(&old_repo_mount_point).join(repo_prefix);
        let new_project_root = PathBuf::from(&repo_mount_point).join(repo_prefix);

        wait_for_repo_mount_ready(&old_project_root).await?;
        wait_for_repo_mount_ready(&new_project_root).await?;
        ensure_local_buck_out(&old_project_root)?;
        ensure_local_buck_out(&new_project_root)?;

        match get_build_targets(
            old_project_root.to_str().unwrap_or(&old_repo_mount_point),
            new_project_root.to_str().unwrap_or(&repo_mount_point),
            changes.clone(),
        )
        .await
        {
            Ok(found_targets) => {
                old_project_root_saved = Some(old_project_root.clone());
                mount_point = Some(repo_mount_point);
                mount_guard = Some(guard);
                mount_guard_old_repo = Some(guard_old_repo);
                targets = found_targets;
                break;
            }
            Err(e) => {
                cleanup_local_buck_out(&new_project_root);
                cleanup_local_buck_out(&old_project_root);
                guard.unmount().await;
                guard_old_repo.unmount().await;
                let retryable = is_retryable_target_discovery_error(&e);
                last_targets_error = Some(e);
                if attempt == MAX_TARGETS_ATTEMPTS || !retryable {
                    break;
                }
                tracing::warn!(
                    "[Task {}] Failed to get build targets (attempt {}/{}): {}. Retrying with fresh mounts...",
                    id,
                    attempt,
                    MAX_TARGETS_ATTEMPTS,
                    last_targets_error.as_ref().unwrap()
                );
            }
        }
    }

    let mount_point = match mount_point {
        Some(value) => value,
        None => {
            let err = last_targets_error
                .map(|e| anyhow!("Error getting build targets: {e}"))
                .unwrap_or_else(|| anyhow!("Error getting build targets: mount point missing"));
            let error_msg = err.to_string();
            if sender
                .send(WSMessage::TaskBuildOutput {
                    build_id: id.clone(),
                    output: error_msg.clone(),
                })
                .is_err()
            {
                tracing::error!(
                    "[Task {}] Failed to send BuildOutput for target discovery error",
                    id
                );
            }
            return Err(err.into());
        }
    };
    let mount_guard = mount_guard.ok_or("Mount guard missing after target discovery")?;
    let mount_guard_old_repo =
        mount_guard_old_repo.ok_or("Old repo mount guard missing after target discovery")?;
    let old_project_root =
        old_project_root_saved.ok_or("Old repo project root missing after target discovery")?;

    let build_result = async {
        // Run buck2 build from the sub-project directory, not the monorepo root.
        // This ensures buck2 uses the sub-project's .buckconfig and PACKAGE files.
        let project_root = PathBuf::from(&mount_point).join(repo_prefix);
        let isolation_dir = buck2_isolation_dir(&project_root)?;
        let mut cmd = Command::new("buck2");
        // --event-log and --build-report are used to collect build execution status
        // at target level (e.g. pending / running / succeeded / failed).
        cmd.args([
            "--isolation-dir", &isolation_dir,
            "--event-log", EVENT_LOG_FILE,
        ]);
        let cmd = cmd
            .arg("build")
            .args(&targets)
            .arg("--target-platforms")
            .arg("prelude//platforms:default")
            // Avoid failing the whole build when a target is explicitly incompatible
            // with the selected platform (e.g., macOS-only crates on Linux builders).
            .arg("--skip-incompatible-targets")
            .arg("--verbose=2")
            .current_dir(&project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        tracing::debug!("[Task {}] Executing command: {:?}", id, cmd);

        let mut child = cmd.spawn()?;

        if let Err(e) = sender.send(WSMessage::TaskPhaseUpdate {
            build_id: id.clone(),
            phase: TaskPhase::RunningBuild,
        }) {
            tracing::error!("Failed to send RunningBuild phase update: {}", e);
        }

        let target_build_track = start_build_status_tracker(&project_root, sender.clone(), cl_trimmed, task_id);

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut stdout_reader = tokio::io::BufReader::new(stdout).lines();
        let mut stderr_reader = tokio::io::BufReader::new(stderr).lines();

        let mut exit_status: Option<ExitStatus> = None;
        loop {
            tokio::select! {
                result = stdout_reader.next_line() => {
                    match result {
                        Ok(Some(line)) => {
                            if sender.send(WSMessage::TaskBuildOutput { build_id: id.clone(), output: line }).is_err() {
                                child.kill().await?;
                                return Err("WebSocket connection lost during build.".into());
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!("[Task {}] Error reading stdout: {}", id, e);
                            break;
                        }
                    }
                },
                result = stderr_reader.next_line() => {
                    match result {
                        Ok(Some(line)) => {
                            if sender.send(WSMessage::TaskBuildOutput { build_id: id.clone(), output: line }).is_err() {
                                child.kill().await?;
                                return Err("WebSocket connection lost during build.".into());
                            }
                        },
                        Ok(None) => break,
                        Err(e) => {
                            tracing::error!("[Task {}] Error reading stderr: {}", id, e);
                            break;
                        },
                    }
                },
                status = child.wait() => {
                    let status = status?;
                    tracing::info!("[Task {}] Buck2 process finished with status: {}", id, status);
                    exit_status = Some(status);
                    break;
                }
            }
        }

        let status = match exit_status {
            Some(status) => status,
            None => child.wait().await?,
        };
        cleanup_buck2_daemon(&project_root);
        target_build_track.cancellation.cancel();
        let _ = target_build_track.tail_handle.await;
        let _ = target_build_track.process_handle.await;
        tracing::info!("Target build status track finished");
        Ok(status)
    }
    .await;

    let new_project_root = PathBuf::from(&mount_point).join(repo_prefix);
    cleanup_local_buck_out(&new_project_root);
    cleanup_local_buck_out(&old_project_root);
    mount_guard.unmount().await;
    mount_guard_old_repo.unmount().await;

    build_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_ensure_local_buck_out_creates_symlink() {
        let repo_path =
            std::env::temp_dir().join(format!("orion-local-buck-out-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&repo_path).unwrap();

        let local_buck_out = ensure_local_buck_out(&repo_path).unwrap();
        let mounted_buck_out = repo_path.join("buck-out");

        assert!(local_buck_out.exists());
        let metadata = std::fs::symlink_metadata(&mounted_buck_out).unwrap();
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&mounted_buck_out).unwrap(),
            local_buck_out
        );

        cleanup_local_buck_out(&repo_path);
        std::fs::remove_dir_all(&repo_path).unwrap();
    }

    #[test]
    fn test_retryable_target_discovery_error_detects_fuse_disconnects() {
        assert!(is_retryable_target_discovery_error(&anyhow!(
            "Fail to get cells: Transport endpoint is not connected (os error 107)"
        )));
        assert!(is_retryable_target_discovery_error(&anyhow!(
            "buck2 targets failed: Stale file handle (os error 116)"
        )));
        assert!(!is_retryable_target_discovery_error(&anyhow!(
            "logical config mismatch"
        )));
    }

    #[tokio::test]
    async fn test_wait_for_repo_mount_ready_retries_until_dir_exists() {
        let base = std::env::temp_dir().join(format!("orion-mount-ready-{}", Uuid::new_v4()));
        let repo_path = base.join("repo");
        let repo_path_for_task = repo_path.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::create_dir_all(&repo_path_for_task).unwrap();
            std::fs::write(repo_path_for_task.join(".buckconfig"), b"[repositories]\n").unwrap();
        });

        wait_for_repo_mount_ready_with_retry(
            &repo_path,
            Duration::from_secs(2),
            Duration::from_millis(10),
        )
        .await
        .unwrap();

        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn test_wait_for_repo_mount_ready_times_out_for_missing_dir() {
        let repo_path = std::env::temp_dir()
            .join(format!("orion-mount-timeout-{}", Uuid::new_v4()))
            .join("repo");

        let err = wait_for_repo_mount_ready_with_retry(
            &repo_path,
            Duration::from_millis(60),
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("Timed out waiting for mounted repo path")
        );
    }

    #[tokio::test]
    async fn test_mount_guard_creation() {
        let mount_guard = MountGuard::new("test_mount_id".to_string(), "test_task_id".to_string());
        assert_eq!(mount_guard.mount_id, "test_mount_id");
        assert_eq!(mount_guard.task_id, "test_task_id");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires buck2, /dev/fuse, network access, and SCORPIO_CONFIG"]
    async fn test_real_mount_waits_until_buck2_cells_succeeds() {
        async fn run_buck2_command(
            repo_path: &Path,
            timeout_secs: u64,
            args: &[&str],
        ) -> std::process::Output {
            tokio::process::Command::new("timeout")
                .kill_on_drop(true)
                .arg(format!("{timeout_secs}s"))
                .arg("buck2")
                .args(args)
                .current_dir(repo_path)
                .output()
                .await
                .unwrap()
        }

        if !Path::new("/dev/fuse").exists() {
            return;
        }
        if std::env::var_os("SCORPIO_CONFIG").is_none() {
            return;
        }

        let job_id = format!("codex-mount-ready-{}", Uuid::new_v4());
        let (mountpoint, mount_id) = mount_antares_fs(&job_id, None).await.unwrap();
        let project_root = PathBuf::from(&mountpoint);
        let isolation_dir = "codex-mount-ready-test";

        wait_for_repo_mount_ready(&project_root).await.unwrap();
        ensure_local_buck_out(&project_root).unwrap();

        let audit_cells = run_buck2_command(
            &project_root,
            120,
            &[
                "--isolation-dir",
                isolation_dir,
                "audit",
                "cell",
                "--json",
                "--reuse-current-config",
            ],
        )
        .await;

        let audit_config = if audit_cells.status.success() {
            Some(
                run_buck2_command(
                    &project_root,
                    120,
                    &[
                        "--isolation-dir",
                        isolation_dir,
                        "audit",
                        "config",
                        "--reuse-current-config",
                    ],
                )
                .await,
            )
        } else {
            None
        };

        let targets = if audit_config
            .as_ref()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            Some(
                run_buck2_command(
                    &project_root,
                    120,
                    &[
                        "--isolation-dir",
                        isolation_dir,
                        "targets",
                        "prelude//platforms:default",
                        "--json-lines",
                    ],
                )
                .await,
            )
        } else {
            None
        };

        let build_output = if targets
            .as_ref()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            Some(
                run_buck2_command(
                    &project_root,
                    120,
                    &[
                        "--isolation-dir",
                        isolation_dir,
                        "build",
                        "prelude//platforms:default",
                    ],
                )
                .await,
            )
        } else {
            None
        };

        cleanup_buck2_daemon(&project_root);
        cleanup_local_buck_out(&project_root);
        let _ = unmount_antares_fs(&mount_id).await;

        assert!(
            audit_cells.status.success(),
            "buck2 audit cell failed. stdout={} stderr={}",
            String::from_utf8_lossy(&audit_cells.stdout),
            String::from_utf8_lossy(&audit_cells.stderr)
        );

        let audit_config =
            audit_config.expect("buck2 audit config was skipped after audit cell failure");
        assert!(
            audit_config.status.success(),
            "buck2 audit config failed. stdout={} stderr={}",
            String::from_utf8_lossy(&audit_config.stdout),
            String::from_utf8_lossy(&audit_config.stderr)
        );

        let targets = targets.expect("buck2 targets was skipped after audit config failure");
        assert!(
            targets.status.success(),
            "buck2 targets failed. stdout={} stderr={}",
            String::from_utf8_lossy(&targets.stdout),
            String::from_utf8_lossy(&targets.stderr)
        );

        let build_output = build_output.expect("buck2 build was skipped after targets failure");
        assert!(
            build_output.status.success(),
            "buck2 build failed. stdout={} stderr={}",
            String::from_utf8_lossy(&build_output.stdout),
            String::from_utf8_lossy(&build_output.stderr)
        );
    }

    // Note: mount/unmount tests removed - they now use scorpiofs direct calls
    // which require actual filesystem setup. See integration tests instead.
}
