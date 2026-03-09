use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use api_model::buck2::{
    status::Status,
    types::{ProjectRelativePath, TaskPhase},
    ws::WSMessage,
};
use chrono::FixedOffset;
use dashmap::DashMap;
use rand::Rng;
use sea_orm::{DatabaseConnection, EntityTrait, prelude::DateTimeUtc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, mpsc::UnboundedSender};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    api::CoreWorkerStatus,
    auto_retry::AutoRetryJudger,
    model::{
        builds,
        targets::{self, TargetState},
    },
};

const DEFAULT_LEASE_TIMEOUT: Duration = Duration::from_secs(30);

/// Request payload for creating a new build task
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BuildRequest {
    pub changes: Vec<Status<ProjectRelativePath>>,
    /// Buck2 target path (e.g. //app:server). Optional for backward compatibility.
    #[serde(default, alias = "target_path")]
    pub target: Option<String>,
}

impl BuildRequest {
    #[allow(dead_code)]
    /// Return requested target path; fallback to "//..." for backward compatibility.
    pub fn target_path(&self) -> String {
        self.target
            .as_ref()
            .cloned()
            .unwrap_or_else(|| "//...".to_string())
    }
}

/// Task queue configuration
#[derive(Debug, Clone)]
pub struct TaskQueueConfig {
    /// Maximum queue length
    pub max_queue_size: usize,
    /// Maximum wait time for tasks in queue
    pub max_wait_time: Duration,
    /// Queue cleanup interval
    pub cleanup_interval: Duration,
}

impl Default for TaskQueueConfig {
    fn default() -> Self {
        Self {
            max_queue_size: 1000,
            max_wait_time: Duration::from_secs(300), // 5 minutes
            cleanup_interval: Duration::from_secs(30), // Cleanup every 30 seconds
        }
    }
}

/// Simple FIFO task queue
#[derive(Debug)]
pub struct TaskQueue {
    /// Queue storage (FIFO)
    queue: VecDeque<PendingBuildEvent>,
    /// Queue configuration
    config: TaskQueueConfig,
}

impl TaskQueue {
    pub fn new(config: TaskQueueConfig) -> Self {
        Self {
            queue: VecDeque::new(),
            config,
        }
    }

    /// Add task-bound build to the end of queue
    pub fn enqueue(&mut self, task: PendingBuildEvent) -> Result<(), String> {
        // Check if queue is full
        if self.queue.len() >= self.config.max_queue_size {
            return Err("Queue is full".to_string());
        }

        self.queue.push_back(task);
        Ok(())
    }

    /// Add task-bound build to the front of queue (used by lease recovery).
    pub fn enqueue_front(&mut self, task: PendingBuildEvent) -> Result<(), String> {
        if self.queue.len() >= self.config.max_queue_size {
            return Err("Queue is full".to_string());
        }

        self.queue.push_front(task);
        Ok(())
    }

    /// Check whether queue already has the given build id.
    pub fn contains_build_id(&self, build_id: Uuid) -> bool {
        self.queue
            .iter()
            .any(|task| task.event_payload.build_event_id == build_id)
    }

    /// Remove task-bound build from the front of queue
    pub fn dequeue(&mut self) -> Option<PendingBuildEvent> {
        self.queue.pop_front()
    }

    /// Clean up expired task-bound build
    pub fn cleanup_expired(&mut self) -> Vec<PendingBuildEvent> {
        let now = Instant::now();
        let mut expired_tasks = Vec::new();

        self.queue.retain(|task| {
            if now.duration_since(task.created_at) > self.config.max_wait_time {
                expired_tasks.push(task.clone());
                false
            } else {
                true
            }
        });

        expired_tasks
    }

    /// Get queue statistics
    pub fn get_stats(&self) -> TaskQueueStats {
        TaskQueueStats {
            total_queued: self.queue.len(),
            leased_builds: 0,
            oldest_task_age_seconds: self
                .queue
                .front()
                .map(|task| Instant::now().duration_since(task.created_at).as_secs()),
        }
    }
}

/// Queue statistics
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TaskQueueStats {
    pub total_queued: usize,
    pub leased_builds: usize,
    /// Age of oldest task in seconds
    pub oldest_task_age_seconds: Option<u64>,
}

/// Mandatory Information for building a task
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildEventPayload {
    pub build_event_id: Uuid,
    pub task_id: Uuid,
    pub cl_link: String,
    pub repo: String,
    pub retry_count: i32,
}

/// Pending task waiting for dispatch
#[derive(Debug, Clone)]
pub struct PendingBuildEvent {
    pub event_payload: BuildEventPayload,
    pub target_id: Option<Uuid>,
    pub target_path: Option<String>,
    pub changes: Vec<Status<ProjectRelativePath>>,
    pub created_at: Instant,
}

/// Information for an active model
#[derive(Clone)]
pub struct BuildInfo {
    pub event_payload: BuildEventPayload,
    pub target_id: Uuid,
    pub target_path: String,
    pub changes: Vec<Status<ProjectRelativePath>>,
    #[allow(dead_code)]
    pub started_at: DateTimeUtc,
    pub auto_retry_judger: AutoRetryJudger,
    #[allow(dead_code)]
    pub worker_id: String,
}

#[derive(Debug, Clone)]
pub struct LeasedBuild {
    pub worker_id: String,
    pub pending_build_event: PendingBuildEvent,
    pub leased_at: Instant,
}

impl BuildEventPayload {
    pub fn new(
        build_event_id: Uuid,
        task_id: Uuid,
        cl_link: String,
        repo: String,
        retry_count: i32,
    ) -> Self {
        Self {
            build_event_id,
            task_id,
            cl_link,
            repo,
            retry_count,
        }
    }
}

/// Status of a worker node
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
pub enum WorkerStatus {
    Idle,
    Busy {
        // Cont ains build ID when busy
        build_id: String,
        // Show task phase when needed
        phase: Option<TaskPhase>,
    },
    Error(String), // Contains fail message
    Lost,          // Heartbeat timeout
}

impl WorkerStatus {
    pub fn status_type(&self) -> CoreWorkerStatus {
        match self {
            WorkerStatus::Idle => CoreWorkerStatus::Idle,
            WorkerStatus::Busy { .. } => CoreWorkerStatus::Busy,
            WorkerStatus::Error(_) => CoreWorkerStatus::Error,
            WorkerStatus::Lost => CoreWorkerStatus::Lost,
        }
    }
}

/// Information about a connected worker
#[derive(Debug)]
pub struct WorkerInfo {
    pub sender: UnboundedSender<WSMessage>,
    pub status: WorkerStatus,
    pub last_heartbeat: DateTimeUtc,
    pub hostname: String,
    pub start_time: DateTimeUtc,
    pub orion_version: String,
}

/// Task scheduler - manages task queue and worker assignment
#[derive(Clone)]
pub struct TaskScheduler {
    /// Pending task queue
    pub pending_tasks: Arc<Mutex<TaskQueue>>,
    /// Event notifier for new tasks or available workers
    pub task_notifier: Arc<Notify>,
    /// Worker information
    pub workers: Arc<DashMap<String, WorkerInfo>>,
    /// Active build tasks
    pub active_builds: Arc<DashMap<String, BuildInfo>>,
    /// Build tasks that have been dispatched but not yet acknowledged by worker
    pub leased_builds: Arc<DashMap<String, LeasedBuild>>,
    /// Lease timeout for unacknowledged builds
    pub lease_timeout: Duration,
    /// Database connection
    pub conn: DatabaseConnection,
}

/// Errors when reading a log segment
#[allow(dead_code)]
#[derive(Debug)]
pub enum LogReadError {
    NotFound,
    OffsetOutOfRange { size: u64 },
    Io(std::io::Error),
}

impl From<std::io::Error> for LogReadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl TaskScheduler {
    /// Create new task scheduler instance
    pub fn new(
        conn: DatabaseConnection,
        workers: Arc<DashMap<String, WorkerInfo>>,
        active_builds: Arc<DashMap<String, BuildInfo>>,
        queue_config: Option<TaskQueueConfig>,
    ) -> Self {
        let config = queue_config.unwrap_or_default();
        let lease_timeout = std::env::var("ORION_LEASE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_LEASE_TIMEOUT);

        Self {
            pending_tasks: Arc::new(Mutex::new(TaskQueue::new(config))),
            task_notifier: Arc::new(Notify::new()),
            workers,
            active_builds,
            leased_builds: Arc::new(DashMap::new()),
            lease_timeout,
            conn,
        }
    }

    /// Ensure target exists for the given task and target path, return the target model
    ///
    /// Creates a new target if not exists
    pub async fn ensure_target(
        &self,
        task_id: Uuid,
        target_path: &str,
    ) -> Result<targets::Model, sea_orm::DbErr> {
        // Find-or-create target for (task_id, target_path)
        targets::Entity::find_or_create(&self.conn, task_id, target_path.to_string()).await
    }

    /// Bound corresponding task build ID to the given task and enqueue
    /// Used when idle worker is not available
    pub async fn enqueue_task(
        &self,
        task_id: Uuid,
        cl_link: &str,
        repo: String,
        changes: Vec<Status<ProjectRelativePath>>,
        target_path: Option<String>,
        retry_count: i32,
    ) -> Result<Uuid, String> {
        let build_event_id = Uuid::now_v7();

        self.enqueue_task_with_build_id(
            build_event_id,
            task_id,
            cl_link,
            repo,
            changes,
            target_path.unwrap_or_default(),
            retry_count,
        )
        .await?;

        Ok(build_event_id)
    }

    /// Enqueue task build with given BuildEvent ID
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_task_with_build_id(
        &self,
        build_event_id: Uuid,
        task_id: Uuid,
        cl_link: &str,
        repo: String,
        changes: Vec<Status<ProjectRelativePath>>,
        target_path: String,
        retry_count: i32,
    ) -> Result<(), String> {
        let build_id_str = build_event_id.to_string();

        if self.active_builds.contains_key(&build_id_str)
            || self.leased_builds.contains_key(&build_id_str)
        {
            return Err(format!("Build {} is already in-flight", build_event_id));
        }

        {
            let queue = self.pending_tasks.lock().await;
            if queue.contains_build_id(build_event_id) {
                return Err(format!("Build {} is already queued", build_event_id));
            }
        }

        // TODO: replace with the new target model
        let target_model = self
            .ensure_target(task_id, &target_path)
            .await
            .map_err(|e| e.to_string())?;

        crate::model::build_records::ensure_orion_task_record(
            &self.conn,
            task_id,
            cl_link,
            &repo,
            &changes,
        )
        .await
        .map_err(|e| e.to_string())?;

        crate::model::build_records::ensure_build_records(
            &self.conn,
            build_event_id,
            task_id,
            target_model.id,
            &repo,
        )
        .await
        .map_err(|e| e.to_string())?;

        let event = BuildEventPayload::new(
            build_event_id,
            task_id,
            cl_link.to_string(),
            repo,
            retry_count,
        );

        let pending_build_event = PendingBuildEvent {
            event_payload: event,
            target_id: Some(target_model.id),
            target_path: Some(target_path),
            changes,
            created_at: Instant::now(),
        };

        {
            let mut queue = self.pending_tasks.lock().await;
            queue.enqueue(pending_build_event)?;
        }

        // Notify that there is a new task to process
        self.task_notifier.notify_one();
        Ok(())
    }

    /// Get queue statistics
    pub async fn get_queue_stats(&self) -> TaskQueueStats {
        let queue = self.pending_tasks.lock().await;
        let mut stats = queue.get_stats();
        stats.leased_builds = self.leased_builds.len();
        stats
    }

    /// Clean up expired task-bound builds
    pub async fn cleanup_expired_tasks(&self) -> Vec<PendingBuildEvent> {
        let mut queue = self.pending_tasks.lock().await;
        queue.cleanup_expired()
    }

    pub async fn is_build_queued(&self, build_id: Uuid) -> bool {
        let queue = self.pending_tasks.lock().await;
        queue.contains_build_id(build_id)
    }

    pub fn is_build_leased(&self, build_id: &str) -> bool {
        self.leased_builds.contains_key(build_id)
    }

    pub fn clear_lease(&self, build_id: &str) {
        self.leased_builds.remove(build_id);
    }

    pub fn register_lease(&self, worker_id: String, pending_build_event: PendingBuildEvent) {
        self.leased_builds.insert(
            pending_build_event.event_payload.build_event_id.to_string(),
            LeasedBuild {
                worker_id,
                pending_build_event,
                leased_at: Instant::now(),
            },
        );
    }

    pub async fn on_task_ack(&self, worker_id: &str, build_id: &str, success: bool, message: &str) {
        if success {
            if self.leased_builds.remove(build_id).is_some() {
                tracing::info!(
                    "Build {} acknowledged by worker {} and lease is cleared.",
                    build_id,
                    worker_id
                );
            }
            return;
        }

        tracing::warn!(
            "Build {} was rejected by worker {}. message={}",
            build_id,
            worker_id,
            message
        );

        if let Some((_, lease)) = self.leased_builds.remove(build_id) {
            self.active_builds.remove(build_id);

            if let Some(mut worker) = self.workers.get_mut(worker_id)
                && let WorkerStatus::Busy {
                    build_id: busy_build,
                    ..
                } = &worker.status
                && busy_build == build_id
            {
                worker.status = WorkerStatus::Idle;
            }

            let mut queue = self.pending_tasks.lock().await;
            if let Err(err) = queue.enqueue_front(lease.pending_build_event.clone()) {
                tracing::error!("Failed to requeue rejected build {}: {}", build_id, err);
            } else {
                tracing::warn!(
                    "Build {} has been requeued after rejection by worker {}.",
                    build_id,
                    worker_id
                );
                self.task_notifier.notify_one();
            }
        }
    }

    pub async fn reclaim_expired_leases(&self) -> usize {
        let now = Instant::now();
        let mut expired_ids = Vec::new();

        for entry in self.leased_builds.iter() {
            if now.duration_since(entry.value().leased_at) >= self.lease_timeout {
                expired_ids.push(entry.key().clone());
            }
        }

        let mut reclaimed = 0;
        for build_id in expired_ids {
            if let Some((_, lease)) = self.leased_builds.remove(&build_id) {
                tracing::warn!(
                    "Build {} lease expired after {:?}; requeueing task.",
                    build_id,
                    self.lease_timeout
                );

                self.active_builds.remove(&build_id);

                if let Some(mut worker) = self.workers.get_mut(&lease.worker_id)
                    && let WorkerStatus::Busy {
                        build_id: busy_build,
                        ..
                    } = &worker.status
                    && busy_build == &build_id
                {
                    worker.status = WorkerStatus::Idle;
                }

                let mut queue = self.pending_tasks.lock().await;
                if queue.enqueue_front(lease.pending_build_event).is_ok() {
                    reclaimed += 1;
                }
            }
        }

        if reclaimed > 0 {
            self.task_notifier.notify_one();
        }

        reclaimed
    }

    /// Check if there are available workers
    pub fn has_idle_workers(&self) -> bool {
        self.workers
            .iter()
            .any(|entry| matches!(entry.value().status, WorkerStatus::Idle))
    }

    /// Get list of idle workers
    pub fn get_idle_workers(&self) -> Vec<String> {
        self.workers
            .iter()
            .filter(|entry| matches!(entry.value().status, WorkerStatus::Idle))
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Search available worker and claim the worker for current build
    #[allow(dead_code)]
    pub fn search_and_claim_worker(&self, build_id: &str) -> Option<String> {
        let idle_workers: Vec<String> = self
            .workers
            .iter()
            .filter(|entry| matches!(entry.value().status, WorkerStatus::Idle))
            .map(|entry| entry.key().clone())
            .collect();
        let chosen_worker_idx = {
            let mut rng = rand::rng();
            rng.random_range(0..idle_workers.len())
        };
        let chosen_worker_id = idle_workers[chosen_worker_idx].clone();
        if let Some(mut worker) = self.workers.get_mut(&chosen_worker_id) {
            worker.status = WorkerStatus::Busy {
                build_id: build_id.to_string(),
                phase: None,
            };
            Some(chosen_worker_id)
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub async fn release_worker(&self, worker_id: &str) {
        tracing::info!("Releasing worker {} back to idle", worker_id);
        if let Some(mut worker) = self.workers.get_mut(worker_id) {
            worker.status = WorkerStatus::Idle;
        }
    }

    /// Try to dispatch queued task-bound builds (concurrent safe)
    pub async fn process_pending_tasks(&self) {
        // Get available workers
        let idle_workers = self.get_idle_workers();
        if idle_workers.is_empty() {
            return;
        }

        // Process tasks in batches, up to the number of idle workers
        let max_tasks = idle_workers.len();
        let mut tasks_to_dispatch = Vec::with_capacity(max_tasks);

        // Batch dequeue tasks
        {
            let mut queue = self.pending_tasks.lock().await;
            for _ in 0..max_tasks {
                if let Some(task) = queue.dequeue() {
                    tasks_to_dispatch.push(task);
                } else {
                    break;
                }
            }
        }

        // Dispatch tasks concurrently
        if !tasks_to_dispatch.is_empty() {
            let dispatch_futures: Vec<_> = tasks_to_dispatch
                .into_iter()
                .map(|task| {
                    let scheduler = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = scheduler.dispatch_task(task).await {
                            tracing::error!("Failed to dispatch queued task: {}", e);
                        }
                    })
                })
                .collect();

            // Wait for all dispatch tasks to complete
            for future in dispatch_futures {
                let _ = future.await;
            }
        }
    }

    /// Dispatch single task
    async fn dispatch_task(&self, pending_build_event: PendingBuildEvent) -> Result<(), String> {
        let idle_workers = self.get_idle_workers();
        if idle_workers.is_empty() {
            return Err("No idle workers available".to_string());
        }

        // Randomly select an idle worker
        let chosen_index = {
            let mut rng = rand::rng();
            rng.random_range(0..idle_workers.len())
        };
        let chosen_id = idle_workers[chosen_index].clone();
        let start_at = chrono::Utc::now();
        let start_at_tz = start_at.with_timezone(&FixedOffset::east_opt(0).unwrap());

        // Create build information
        let build_info = BuildInfo {
            event_payload: pending_build_event.event_payload.clone(),
            changes: pending_build_event.changes.clone(),
            target_id: pending_build_event.target_id.unwrap_or(Uuid::nil()),
            target_path: pending_build_event.target_path.clone().unwrap_or_default(),
            worker_id: chosen_id.clone(),
            auto_retry_judger: AutoRetryJudger::new(),
            started_at: start_at,
        };

        let build_id = pending_build_event.event_payload.build_event_id;
        let build_id_str = build_id.to_string();

        // Ensure build record exists (queueing path persists first, lease recovery may re-dispatch).
        if builds::Entity::find_by_id(build_id)
            .one(&self.conn)
            .await
            .map_err(|e| e.to_string())?
            .is_none()
        {
            builds::Model::insert_build(
                build_id,
                pending_build_event.event_payload.task_id,
                pending_build_event.target_id.unwrap_or(Uuid::nil()),
                pending_build_event.event_payload.repo.clone(),
                &self.conn,
            )
            .await
            .map_err(|e| e.to_string())?;
        }

        // Create WebSocket message
        let msg = WSMessage::TaskBuild {
            build_id: build_id_str.clone(),
            repo: pending_build_event.event_payload.repo.clone(),
            cl_link: pending_build_event.event_payload.cl_link.clone(),
            changes: pending_build_event.changes.clone(),
        };

        // Send task to worker
        if let Some(mut worker) = self.workers.get_mut(&chosen_id) {
            if worker.sender.send(msg).is_ok() {
                // Only mark Building after send succeeds
                if let Err(e) = targets::update_state(
                    &self.conn,
                    pending_build_event.target_id.unwrap_or(Uuid::nil()),
                    TargetState::Building,
                    Some(start_at_tz),
                    None,
                    None,
                )
                .await
                {
                    tracing::warn!("update target state failed: {e}");
                }

                worker.status = WorkerStatus::Busy {
                    build_id: build_id_str.clone(),
                    phase: None,
                };
                self.active_builds.insert(build_id_str.clone(), build_info);
                self.register_lease(chosen_id.clone(), pending_build_event.clone());

                tracing::info!(
                    "Queued task {}/{} dispatched to worker {} (lease started)",
                    pending_build_event.event_payload.task_id,
                    build_id,
                    chosen_id
                );
                Ok(())
            } else {
                // Send failed: best-effort mark target back to Pending
                let _ = targets::update_state(
                    &self.conn,
                    pending_build_event.target_id.unwrap_or(Uuid::nil()),
                    TargetState::Pending,
                    Some(start_at_tz),
                    None,
                    None,
                )
                .await
                .map_err(|e| tracing::warn!("update target rollback failed: {e}"));
                Err(format!("Failed to send task to worker {chosen_id}"))
            }
        } else {
            Err(format!("Worker {chosen_id} not found"))
        }
    }

    /// Notify about new task or available worker
    pub fn notify_task_available(&self) {
        self.task_notifier.notify_one();
    }

    /// Start queue management background task (event-driven + periodic cleanup)
    pub async fn start_queue_manager(self) {
        let cleanup_interval = {
            let queue = self.pending_tasks.lock().await;
            queue.config.cleanup_interval
        };

        // Task dispatcher: wait for notifications or process periodically
        let dispatch_scheduler = self.clone();
        let dispatch_task = tokio::spawn(async move {
            loop {
                // Wait for notification or timeout
                tokio::select! {
                    // Wait for new task or worker available notification
                    _ = dispatch_scheduler.task_notifier.notified() => {
                        dispatch_scheduler.process_pending_tasks().await;
                    }
                    // Periodic check (prevent missing notifications)
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {
                        dispatch_scheduler.process_pending_tasks().await;
                    }
                }
            }
        });

        // Cleaner: periodically clean up expired tasks
        let cleanup_scheduler = self.clone();
        let cleanup_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(cleanup_interval);

            loop {
                interval.tick().await;

                // Clean up expired tasks
                let expired_tasks = cleanup_scheduler.cleanup_expired_tasks().await;
                if !expired_tasks.is_empty() {
                    tracing::warn!(
                        "Cleaned up {} expired tasks from queue",
                        expired_tasks.len()
                    );

                    // Log expired task information
                    for task in expired_tasks {
                        tracing::debug!(
                            "Expired build: {}/{} ({})",
                            task.event_payload.task_id,
                            task.event_payload.build_event_id,
                            task.event_payload.repo
                        );
                    }
                }
            }
        });

        // Lease reclaimer: recover tasks that were dispatched but never acknowledged.
        let lease_scheduler = self.clone();
        let lease_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            loop {
                interval.tick().await;
                let reclaimed = lease_scheduler.reclaim_expired_leases().await;
                if reclaimed > 0 {
                    tracing::warn!(
                        "Recovered {} expired leases and requeued corresponding tasks",
                        reclaimed
                    );
                }
            }
        });

        // Wait for tasks to complete (actually runs forever)
        tokio::select! {
            _ = dispatch_task => {
                tracing::error!("Task dispatcher unexpectedly stopped");
            }
            _ = cleanup_task => {
                tracing::error!("Task cleanup unexpectedly stopped");
            }
            _ = lease_task => {
                tracing::error!("Lease recovery unexpectedly stopped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test task queue basic functionality
    #[test]
    fn test_task_queue_fifo() {
        let config = TaskQueueConfig::default();
        let mut queue = TaskQueue::new(config);

        let build_event1 = BuildEventPayload::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            "test_cl_link".to_string(),
            "test/repo".to_string(),
            0,
        );

        // Create test tasks
        let task1 = PendingBuildEvent {
            event_payload: build_event1.clone(),
            target_id: Some(Uuid::now_v7()),
            target_path: Some("//app:server".to_string()),
            changes: vec![],
            created_at: Instant::now(),
        };

        let build_event2 = BuildEventPayload::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            "test_cl_link_2".to_string(),
            "test2/repo".to_string(),
            0,
        );
        let task2 = PendingBuildEvent {
            event_payload: build_event2.clone(),
            target_id: Some(Uuid::now_v7()),
            target_path: Some("//app:server2".to_string()),
            changes: vec![],
            created_at: Instant::now(),
        };

        // Test FIFO behavior
        assert!(queue.enqueue(task1.clone()).is_ok());
        assert!(queue.enqueue(task2.clone()).is_ok());

        let dequeued1 = queue.dequeue().unwrap();
        assert_eq!(
            dequeued1.event_payload.build_event_id,
            task1.event_payload.build_event_id
        );
        assert_eq!(dequeued1.event_payload.repo, "test/repo");

        let dequeued2 = queue.dequeue().unwrap();
        assert_eq!(
            dequeued2.event_payload.build_event_id,
            task2.event_payload.build_event_id
        );
        assert_eq!(dequeued2.event_payload.repo, "test2/repo");
    }

    /// Test queue capacity limit
    #[test]
    fn test_queue_capacity() {
        let config = TaskQueueConfig {
            max_queue_size: 2,
            max_wait_time: Duration::from_secs(60),
            cleanup_interval: Duration::from_secs(30),
        };
        let mut queue = TaskQueue::new(config);

        let build_event = BuildEventPayload::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            "test_cl_link".to_string(),
            "test/repo".to_string(),
            0,
        );
        let task = PendingBuildEvent {
            event_payload: build_event.clone(),
            target_id: Some(Uuid::now_v7()),
            target_path: Some("//app:server".to_string()),
            changes: vec![],
            created_at: Instant::now(),
        };

        // Fill queue to capacity
        assert!(queue.enqueue(task.clone()).is_ok());
        assert!(queue.enqueue(task.clone()).is_ok());

        // Should fail when full
        assert!(queue.enqueue(task).is_err());
    }

    #[test]
    fn test_enqueue_front_priority() {
        let config = TaskQueueConfig::default();
        let mut queue = TaskQueue::new(config);

        let first = PendingBuildEvent {
            event_payload: BuildEventPayload::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                "cl1".to_string(),
                "repo1".to_string(),
                0,
            ),
            target_id: Some(Uuid::now_v7()),
            target_path: Some("//:one".to_string()),
            changes: vec![],
            created_at: Instant::now(),
        };

        let urgent = PendingBuildEvent {
            event_payload: BuildEventPayload::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                "cl2".to_string(),
                "repo2".to_string(),
                0,
            ),
            target_id: Some(Uuid::now_v7()),
            target_path: Some("//:two".to_string()),
            changes: vec![],
            created_at: Instant::now(),
        };

        queue.enqueue(first.clone()).unwrap();
        queue.enqueue_front(urgent.clone()).unwrap();

        assert_eq!(
            queue.dequeue().unwrap().event_payload.build_event_id,
            urgent.event_payload.build_event_id
        );
        assert_eq!(
            queue.dequeue().unwrap().event_payload.build_event_id,
            first.event_payload.build_event_id
        );
    }

    #[test]
    fn test_contains_build_id() {
        let config = TaskQueueConfig::default();
        let mut queue = TaskQueue::new(config);

        let build_id = Uuid::now_v7();
        let task = PendingBuildEvent {
            event_payload: BuildEventPayload::new(
                build_id,
                Uuid::now_v7(),
                "cl".to_string(),
                "repo".to_string(),
                0,
            ),
            target_id: Some(Uuid::now_v7()),
            target_path: Some("//:target".to_string()),
            changes: vec![],
            created_at: Instant::now(),
        };

        assert!(!queue.contains_build_id(build_id));
        queue.enqueue(task).unwrap();
        assert!(queue.contains_build_id(build_id));
    }
}
