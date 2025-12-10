//! Suggested Antares daemon HTTP interface.
//!
//! This module intentionally focuses on the REST surface area and traits that the
//! runtime should implement. All functions are left as `todo!()` placeholders so
//! future changes can fill in the actual orchestration logic without rewriting
//! the API shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::antares::fuse::AntaresFuse;
use crate::dicfuse::Dicfuse;

/// High-level HTTP daemon that exposes Antares orchestration capabilities.
pub struct AntaresDaemon<S: AntaresService> {
    bind_addr: SocketAddr,
    service: Arc<S>,
    shutdown_timeout: Duration,
}

impl<S> AntaresDaemon<S>
where
    S: AntaresService + 'static,
{
    /// Construct a daemon bound to the provided socket and backed by the given service.
    pub fn new(bind_addr: SocketAddr, service: Arc<S>) -> Self {
        Self {
            bind_addr,
            service,
            shutdown_timeout: Duration::from_secs(10),
        }
    }

    /// Override the graceful shutdown timeout applied to the HTTP server.
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Produce an Axum router with all routes wired to their handlers.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(Self::healthcheck))
            .route("/mounts", post(Self::create_mount))
            .route("/mounts", get(Self::list_mounts))
            .route("/mounts/{mount_id}", get(Self::describe_mount))
            .route("/mounts/{mount_id}", delete(Self::delete_mount))
            .with_state(self.service.clone())
    }

    /// Run the HTTP server until it receives a shutdown signal.
    /// Note: For graceful shutdown with mount cleanup, use AntaresDaemon<AntaresServiceImpl>.
    pub async fn serve(self) -> Result<(), ApiError> {
        let router = self.router();

        let listener = tokio::net::TcpListener::bind(self.bind_addr)
            .await
            .map_err(|e| {
                ApiError::Service(ServiceError::Internal(format!(
                    "failed to bind to {}: {}",
                    self.bind_addr, e
                )))
            })?;

        tracing::info!("Antares daemon listening on {}", self.bind_addr);

        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("Received shutdown signal");
            })
            .await
            .map_err(|e| ApiError::Service(ServiceError::Internal(format!("server error: {}", e))))?;

        Ok(())
    }

    /// Lightweight health/liveness probe.
    async fn healthcheck(State(service): State<Arc<S>>) -> impl IntoResponse {
        // For generic S, we return a simple response
        // AntaresServiceImpl has health_info() but we can't call it here
        Json(HealthResponse {
            status: "healthy".to_string(),
            mount_count: 0,
            uptime_secs: 0,
        })
    }

    async fn create_mount(
        State(service): State<Arc<S>>,
        Json(request): Json<CreateMountRequest>,
    ) -> Result<Json<MountCreated>, ApiError> {
        let status = service.create_mount(request).await?;
        Ok(Json(MountCreated {
            mount_id: status.mount_id,
            mountpoint: status.mountpoint,
            state: status.state,
        }))
    }

    async fn list_mounts(State(service): State<Arc<S>>) -> Result<Json<MountCollection>, ApiError> {
        let mounts = service.list_mounts().await?;
        Ok(Json(MountCollection { mounts }))
    }

    async fn describe_mount(
        State(service): State<Arc<S>>,
        Path(mount_id): Path<Uuid>,
    ) -> Result<Json<MountStatus>, ApiError> {
        let status = service.describe_mount(mount_id).await?;
        Ok(Json(status))
    }

    async fn delete_mount(
        State(service): State<Arc<S>>,
        Path(mount_id): Path<Uuid>,
    ) -> Result<Json<MountStatus>, ApiError> {
        let status = service.delete_mount(mount_id).await?;
        Ok(Json(status))
    }
}

/// Asynchronous service boundary that the HTTP layer depends on.
#[async_trait]
pub trait AntaresService: Send + Sync {
    async fn create_mount(&self, request: CreateMountRequest) -> Result<MountStatus, ServiceError>;
    async fn list_mounts(&self) -> Result<Vec<MountStatus>, ServiceError>;
    async fn describe_mount(&self, mount_id: Uuid) -> Result<MountStatus, ServiceError>;
    async fn delete_mount(&self, mount_id: Uuid) -> Result<MountStatus, ServiceError>;
}

/// Request payload for provisioning a new mount.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateMountRequest {
    /// Absolute path where the mount should appear on the host.
    pub mountpoint: String,
    /// Upper (read-write) directory unique per mount.
    pub upper_dir: String,
    /// Optional CL passthrough directory.
    pub cl_dir: Option<String>,
    /// Arbitrary labels that the orchestrator can apply to the mount.
    pub labels: Vec<String>,
    /// Whether the presented filesystem should be mounted read-only.
    pub readonly: bool,
}

/// Summary returned immediately after provisioning succeeds.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountCreated {
    pub mount_id: Uuid,
    pub mountpoint: String,
    pub state: MountLifecycle,
}

/// Snapshot of a single mount's state.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountStatus {
    pub mount_id: Uuid,
    pub mountpoint: String,
    pub layers: MountLayers,
    pub state: MountLifecycle,
    pub created_at_epoch_ms: u64,
    pub last_seen_epoch_ms: u64,
}

/// Convenience wrapper used by list endpoints.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountCollection {
    pub mounts: Vec<MountStatus>,
}

/// Directory layout for a mount.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountLayers {
    pub upper: String,
    pub cl: Option<String>,
    pub dicfuse: String,
}

/// Lifecycle indicator used in responses and service contracts.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum MountLifecycle {
    Provisioning,
    Mounted,
    Unmounting,
    Unmounted,
    Failed { reason: String },
}

/// Health check response payload.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthResponse {
    /// Service health status: "healthy" or "degraded"
    pub status: String,
    /// Current number of active mounts
    pub mount_count: usize,
    /// Service uptime in seconds
    pub uptime_secs: u64,
}

/// Error response body for JSON output.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorBody {
    /// Human-readable error message
    pub error: String,
    /// Machine-readable error code
    pub code: String,
}

/// Service-level failures (implementation specific) that surface through the API.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("mount not found: {0}")]
    NotFound(Uuid),
    #[error("failed to interact with fuse stack: {0}")]
    FuseFailure(String),
    #[error("unexpected error: {0}")]
    Internal(String),
}

/// HTTP-facing errors mapped to responses.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("serde payload rejected: {0}")]
    BadPayload(String),
    #[error("server shutting down")]
    Shutdown,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status_code, error_code, message) = match &self {
            ApiError::Service(ServiceError::InvalidRequest(msg)) => {
                (StatusCode::BAD_REQUEST, "INVALID_REQUEST", msg.clone())
            }
            ApiError::Service(ServiceError::NotFound(id)) => {
                (StatusCode::NOT_FOUND, "NOT_FOUND", format!("mount {} not found", id))
            }
            ApiError::Service(ServiceError::FuseFailure(msg)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "FUSE_ERROR", msg.clone())
            }
            ApiError::Service(ServiceError::Internal(msg)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", msg.clone())
            }
            ApiError::BadPayload(msg) => {
                (StatusCode::BAD_REQUEST, "BAD_PAYLOAD", msg.clone())
            }
            ApiError::Shutdown => {
                (StatusCode::SERVICE_UNAVAILABLE, "SHUTDOWN", "server is shutting down".into())
            }
        };

        let body = ErrorBody {
            error: message,
            code: error_code.to_string(),
        };

        (status_code, Json(body)).into_response()
    }
}

// ============================================================================
// Service Implementation
// ============================================================================

/// Internal entry tracking a single mount.
struct MountEntry {
    mount_id: Uuid,
    request: CreateMountRequest,
    fuse: AntaresFuse,
    state: MountLifecycle,
    created_at_epoch_ms: u64,
    last_seen_epoch_ms: u64,
}

impl MountEntry {
    /// Convert to public MountStatus for API responses.
    fn to_status(&self) -> MountStatus {
        MountStatus {
            mount_id: self.mount_id,
            mountpoint: self.request.mountpoint.clone(),
            layers: MountLayers {
                upper: self.request.upper_dir.clone(),
                cl: self.request.cl_dir.clone(),
                dicfuse: "shared".to_string(),
            },
            state: self.state.clone(),
            created_at_epoch_ms: self.created_at_epoch_ms,
            last_seen_epoch_ms: self.last_seen_epoch_ms,
        }
    }

    /// Update the last_seen timestamp.
    fn update_last_seen(&mut self) {
        self.last_seen_epoch_ms = current_epoch_ms();
    }
}

/// Get current time as milliseconds since UNIX epoch.
fn current_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Concrete implementation of AntaresService.
pub struct AntaresServiceImpl {
    /// Shared Dicfuse instance (read-only base layer).
    dicfuse: Arc<Dicfuse>,
    /// Active mounts indexed by UUID.
    mounts: Arc<RwLock<HashMap<Uuid, MountEntry>>>,
    /// Service start time for uptime calculation.
    start_time: Instant,
}

impl AntaresServiceImpl {
    /// Create a new service instance.
    ///
    /// # Arguments
    /// * `dicfuse` - Optional shared Dicfuse instance. If None, creates a new one.
    ///
    /// # Note
    /// Requires config to be initialized via `config::init_config()` before calling.
    pub async fn new(dicfuse: Option<Arc<Dicfuse>>) -> Self {
        let dic = match dicfuse {
            Some(d) => d,
            None => Arc::new(Dicfuse::new().await),
        };
        Self {
            dicfuse: dic,
            mounts: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
        }
    }

    /// Validate the create mount request.
    fn validate_request(request: &CreateMountRequest) -> Result<(), ServiceError> {
        if request.mountpoint.is_empty() {
            return Err(ServiceError::InvalidRequest(
                "mountpoint cannot be empty".into(),
            ));
        }
        if request.upper_dir.is_empty() {
            return Err(ServiceError::InvalidRequest(
                "upper_dir cannot be empty".into(),
            ));
        }
        // readonly not yet supported - must be false
        if request.readonly {
            return Err(ServiceError::InvalidRequest(
                "readonly mounts are not yet supported".into(),
            ));
        }
        Ok(())
    }

    /// Check if a mountpoint is already in use.
    async fn is_mountpoint_in_use(&self, mountpoint: &str) -> bool {
        let mounts = self.mounts.read().await;
        mounts
            .values()
            .any(|e| e.request.mountpoint == mountpoint)
    }

    /// Get service health information.
    pub async fn health_info(&self) -> HealthResponse {
        let mounts = self.mounts.read().await;
        HealthResponse {
            status: "healthy".to_string(),
            mount_count: mounts.len(),
            uptime_secs: self.start_time.elapsed().as_secs(),
        }
    }

    /// Cleanup all mounts during shutdown.
    pub async fn shutdown_cleanup(&self) -> Result<(), ServiceError> {
        let mut mounts = self.mounts.write().await;
        let mount_ids: Vec<Uuid> = mounts.keys().cloned().collect();

        for mount_id in mount_ids {
            if let Some(mut entry) = mounts.remove(&mount_id) {
                tracing::info!("Unmounting {} during shutdown", mount_id);
                if let Err(e) = entry.fuse.unmount().await {
                    tracing::warn!("Failed to unmount {} during shutdown: {}", mount_id, e);
                    // Continue with other mounts even if one fails
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl AntaresService for AntaresServiceImpl {
    async fn create_mount(&self, request: CreateMountRequest) -> Result<MountStatus, ServiceError> {
        // 1. Validate request
        Self::validate_request(&request)?;

        // 2. Quick check: is mountpoint already in use?
        if self.is_mountpoint_in_use(&request.mountpoint).await {
            return Err(ServiceError::InvalidRequest(format!(
                "mountpoint {} is already in use",
                request.mountpoint
            )));
        }

        // 3. Prepare paths
        let mountpoint = PathBuf::from(&request.mountpoint);
        let upper_dir = PathBuf::from(&request.upper_dir);
        let cl_dir = request.cl_dir.as_ref().map(PathBuf::from);

        // 4. Create AntaresFuse instance (may take time, not holding lock)
        let mut fuse = AntaresFuse::new(mountpoint, self.dicfuse.clone(), upper_dir, cl_dir)
            .await
            .map_err(|e| ServiceError::FuseFailure(format!("failed to create fuse: {}", e)))?;

        // 5. Mount the filesystem
        fuse.mount()
            .await
            .map_err(|e| ServiceError::FuseFailure(format!("failed to mount: {}", e)))?;

        // 6. Generate mount ID and create entry
        let mount_id = Uuid::new_v4();
        let now = current_epoch_ms();

        let entry = MountEntry {
            mount_id,
            request: request.clone(),
            fuse,
            state: MountLifecycle::Mounted,
            created_at_epoch_ms: now,
            last_seen_epoch_ms: now,
        };

        // 7. Double-check and insert (holding write lock)
        let mut mounts = self.mounts.write().await;

        // Re-check mountpoint (another request might have succeeded during step 4-5)
        if mounts
            .values()
            .any(|e| e.request.mountpoint == request.mountpoint)
        {
            // Entry will be dropped, fuse cleanup happens automatically
            return Err(ServiceError::InvalidRequest(format!(
                "mountpoint {} was claimed by another request",
                request.mountpoint
            )));
        }

        let status = entry.to_status();
        mounts.insert(mount_id, entry);

        tracing::info!("Created mount {} at {}", mount_id, request.mountpoint);
        Ok(status)
    }

    async fn list_mounts(&self) -> Result<Vec<MountStatus>, ServiceError> {
        let mounts = self.mounts.read().await;
        let list: Vec<MountStatus> = mounts.values().map(|e| e.to_status()).collect();
        Ok(list)
    }

    async fn describe_mount(&self, mount_id: Uuid) -> Result<MountStatus, ServiceError> {
        let mounts = self.mounts.read().await;
        let entry = mounts.get(&mount_id).ok_or(ServiceError::NotFound(mount_id))?;
        Ok(entry.to_status())
    }

    async fn delete_mount(&self, mount_id: Uuid) -> Result<MountStatus, ServiceError> {
        // Get write lock and remove entry
        let mut mounts = self.mounts.write().await;
        let mut entry = mounts
            .remove(&mount_id)
            .ok_or(ServiceError::NotFound(mount_id))?;

        // Update state
        entry.state = MountLifecycle::Unmounting;
        entry.update_last_seen();

        // Release lock before potentially slow unmount operation
        drop(mounts);

        // Unmount the filesystem
        if let Err(e) = entry.fuse.unmount().await {
            tracing::error!("Failed to unmount {}: {}", mount_id, e);
            entry.state = MountLifecycle::Failed {
                reason: format!("unmount failed: {}", e),
            };
        } else {
            entry.state = MountLifecycle::Unmounted;
        }

        tracing::info!("Deleted mount {}", mount_id);
        Ok(entry.to_status())
    }
}
