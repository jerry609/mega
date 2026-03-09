use api_model::buck2::{
    status::Status,
    types::{BuildOutcome, ProjectRelativePath},
    ws::WSMessage,
};
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::buck_controller;

/// Result of a build operation containing status and metadata.
#[derive(Debug, Serialize)]
pub struct BuildResult {
    /// Whether the build operation was successful
    pub success: bool,
    /// Unique identifier for the build task
    pub build_id: String,
    /// Process exit code (None if not yet completed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Human-readable status or error message
    pub message: String,
    /// Optional semantic outcome for successful non-build completions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<BuildOutcome>,
}

/// Executes a buck build and reports completion via WebSocket.
///
/// # Arguments
/// * `id` - Unique identifier for tracking the build task
/// * `cl_link` - Change list link associated with this build
/// * `repo` - Repository root path
/// * `changes` - File changes associated with this task
/// * `sender` - Channel for sending WebSocket messages
pub async fn buck_build(
    id: Uuid,
    cl_link: String,
    repo: String,
    changes: Vec<Status<ProjectRelativePath>>,
    sender: UnboundedSender<WSMessage>,
) -> BuildResult {
    let id_str = id.to_string();
    tracing::info!("[Task {}] Received build request.", id_str);

    let build_result = match buck_controller::build(
        id_str.clone(),
        repo,
        cl_link,
        sender.clone(),
        changes,
    )
    .await
    {
        Ok(result) => {
            tracing::info!(
                "[Task {}] {}; Exit code: {:?}; outcome: {:?}",
                id_str,
                result.message,
                result.exit_code,
                result.outcome
            );
            BuildResult {
                success: result.success,
                build_id: id_str.clone(),
                exit_code: result.exit_code,
                message: result.message,
                outcome: result.outcome,
            }
        }
        Err(e) => {
            let error_msg = format!("Build execution failed: {e}");
            tracing::error!("[Task {}] {}", id_str, error_msg);
            BuildResult {
                success: false,
                build_id: id_str.clone(),
                exit_code: None,
                message: error_msg,
                outcome: None,
            }
        }
    };

    let complete_msg = WSMessage::TaskBuildComplete {
        build_id: build_result.build_id.clone(),
        success: build_result.success,
        exit_code: build_result.exit_code,
        message: build_result.message.clone(),
        outcome: build_result.outcome.clone(),
    };

    if sender.send(complete_msg).is_err() {
        tracing::error!(
            "[Task {}] Failed to send BuildComplete message. Connection likely lost.",
            id_str
        );
    }

    build_result
}
