//! Antares filesystem management via scorpiofs direct calls.
//!
//! This module provides a singleton wrapper around `scorpiofs::AntaresManager`
//! for managing overlay filesystem mounts used during build operations.

use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use scorpiofs::{AntaresConfig, AntaresManager, AntaresPaths};
use tokio::sync::OnceCell;

static MANAGER: OnceCell<Arc<AntaresManager>> = OnceCell::const_new();
static SCORPIO_CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

type DynError = Box<dyn Error + Send + Sync>;

const BUCK_REPO_READY_PATH: &str = ".buckconfig";
const ORION_WORKER_RUNTIME_DIR: &str = "orion-workers";

/// Get the global AntaresManager instance.
///
/// Initializes the manager on first call by loading the scorpio configuration
/// from the path specified by `SCORPIO_CONFIG` environment variable.
///
/// If `SCORPIO_CONFIG` is not set, Orion will look for `scorpio.toml` in:
/// 1. Current working directory
/// 2. Next to the executable
/// 3. `/etc/scorpio/scorpio.toml` (system default)
///
/// Returns an error if no config file is found.
async fn get_manager() -> Result<&'static Arc<AntaresManager>, DynError> {
    MANAGER
        .get_or_try_init(|| async {
            let config_path = resolve_config_path()?;
            register_config_path(&config_path)?;
            let config_path_str = config_path.to_str().ok_or_else(|| -> DynError {
                Box::new(io_other("Invalid SCORPIO_CONFIG path (non-UTF8)"))
            })?;

            tracing::info!("Initializing Antares with config: {}", config_path_str);
            init_scorpio_config(config_path_str)?;

            let mut paths = AntaresPaths::from_global_config();
            paths.state_file = scoped_state_file(&paths.state_file);
            if let Some(parent) = paths.state_file.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    io_other(format!(
                        "Failed to create Antares state directory {}: {e}",
                        parent.display()
                    ))
                })?;
            }

            let store_path = scoped_store_path(Path::new(scorpiofs::util::config::store_path()));
            fs::create_dir_all(&store_path).map_err(|e| {
                io_other(format!(
                    "Failed to create isolated Dicfuse store {}: {e}",
                    store_path.display()
                ))
            })?;
            let scoped_store = store_path.to_string_lossy().into_owned();
            let worker_scope = worker_scope();
            tracing::info!(
                worker_scope = %worker_scope,
                store_path = %scoped_store,
                state_file = %paths.state_file.display(),
                "Initializing direct Antares manager with worker-scoped state"
            );

            let manager = AntaresManager::new_with_store_path(paths, &scoped_store).await;
            Ok(Arc::new(manager))
        })
        .await
}

fn init_scorpio_config(config_path: &str) -> Result<(), DynError> {
    match scorpiofs::util::config::init_config(config_path) {
        Ok(()) => Ok(()),
        Err(e) if e.contains("already initialized") => {
            tracing::debug!(
                path = %config_path,
                "Scorpio config already initialized in this worker; reusing process-global config"
            );
            Ok(())
        }
        Err(e) => Err(Box::new(io_other(format!(
            "Failed to load scorpio config from {config_path}: {e}. \
Hint: set SCORPIO_CONFIG=/path/to/scorpio.toml or create /etc/scorpio/scorpio.toml"
        )))),
    }
}

fn register_config_path(config_path: &Path) -> Result<(), DynError> {
    if let Some(existing) = SCORPIO_CONFIG_PATH.get() {
        if existing != config_path {
            return Err(Box::new(io_other(format!(
                "Scorpio config path changed within the same worker: {} -> {}",
                existing.display(),
                config_path.display()
            ))));
        }
        return Ok(());
    }

    let _ = SCORPIO_CONFIG_PATH.set(config_path.to_path_buf());
    Ok(())
}

fn io_other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn worker_scope() -> String {
    let raw = std::env::var("ORION_WORKER_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("pid-{}", std::process::id()));
    sanitize_path_component(&raw)
}

fn sanitize_path_component(raw: &str) -> String {
    let mut sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    sanitized.truncate(96);
    sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.is_empty() {
        format!("pid-{}", std::process::id())
    } else {
        sanitized
    }
}

fn scoped_store_path(base_store_path: &Path) -> PathBuf {
    base_store_path
        .join(ORION_WORKER_RUNTIME_DIR)
        .join(worker_scope())
}

fn scoped_state_file(base_state_file: &Path) -> PathBuf {
    let parent = base_state_file
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"));
    let stem = base_state_file
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    let scope = worker_scope();
    match base_state_file.extension().and_then(|value| value.to_str()) {
        Some(ext) if !ext.is_empty() => parent.join(format!("{stem}-{scope}.{ext}")),
        _ => parent.join(format!("{stem}-{scope}")),
    }
}

fn normalized_repo_scope(repo_path: &str) -> String {
    let trimmed = repo_path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "repo-root".to_string();
    }

    let digest = ring::digest::digest(&ring::digest::SHA256, trimmed.as_bytes());
    let suffix = &hex::encode(digest.as_ref())[..12];
    format!("repo-{suffix}")
}

fn mount_role(job_id: &str) -> &'static str {
    if job_id.contains("-old-") {
        "old"
    } else {
        "new"
    }
}

fn scoped_mountpoint(repo_path: &str, job_id: &str) -> PathBuf {
    Path::new(scorpiofs::util::config::antares_mount_root())
        .join(ORION_WORKER_RUNTIME_DIR)
        .join(worker_scope())
        .join(normalized_repo_scope(repo_path))
        .join(mount_role(job_id))
}

fn resolve_config_path() -> Result<PathBuf, DynError> {
    // 1. Check SCORPIO_CONFIG environment variable
    if let Ok(path) = std::env::var("SCORPIO_CONFIG") {
        let config_path = PathBuf::from(&path);
        if config_path.exists() {
            return Ok(config_path);
        }
        return Err(Box::new(io_other(format!(
            "SCORPIO_CONFIG is set but file does not exist: {}",
            config_path.display()
        ))));
    }

    // 2. Check current working directory
    let cwd = std::env::current_dir().map_err(|e| {
        Box::new(io_other(format!(
            "Failed to get current working directory: {e}"
        ))) as DynError
    })?;
    let cwd_candidate = cwd.join("scorpio.toml");
    if cwd_candidate.exists() {
        return Ok(cwd_candidate);
    }

    // 3. Check next to executable
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let exe_candidate = exe_dir.join("scorpio.toml");
        if exe_candidate.exists() {
            return Ok(exe_candidate);
        }
    }

    // 4. Check system default path
    let system_candidate = PathBuf::from("/etc/scorpio/scorpio.toml");
    if system_candidate.exists() {
        return Ok(system_candidate);
    }

    Err(Box::new(io_other(format!(
        "Scorpio config not found. Set SCORPIO_CONFIG=/path/to/scorpio.toml, \
         place scorpio.toml in the working directory ({}), \
         or create /etc/scorpio/scorpio.toml",
        cwd.display()
    ))))
}

/// Mount a job overlay filesystem.
///
/// Creates a new Antares overlay mount for the specified job. The underlying
/// Dicfuse layer provides read-only access to the repository, while the overlay
/// allows copy-on-write modifications.
///
/// # Arguments
/// * `job_id` - Unique identifier for this build job
/// * `cl` - Optional changelist layer name
///
/// # Returns
/// The `AntaresConfig` containing mountpoint and job metadata on success.
pub async fn mount_job(
    job_id: &str,
    repo_path: &str,
    cl: Option<&str>,
) -> Result<AntaresConfig, DynError> {
    let mountpoint = scoped_mountpoint(repo_path, job_id);
    tracing::debug!(
        "Mounting Antares job: job_id={}, repo_path={}, mountpoint={}, cl={:?}",
        job_id,
        repo_path,
        mountpoint.display(),
        cl
    );
    get_manager()
        .await?
        .mount_job_at_for_path_with_ready_path(
            job_id,
            mountpoint,
            repo_path,
            cl,
            Some(BUCK_REPO_READY_PATH),
        )
        .await
        .map_err(Into::into)
}

/// Unmount and cleanup a job overlay filesystem.
///
/// # Arguments
/// * `job_id` - The job identifier to unmount
///
/// # Returns
/// The `AntaresConfig` of the unmounted job if it existed.
pub async fn unmount_job(job_id: &str) -> Result<Option<AntaresConfig>, DynError> {
    tracing::debug!("Unmounting Antares job: job_id={}", job_id);
    get_manager()
        .await?
        .umount_job(job_id)
        .await
        .map_err(Into::into)
}
