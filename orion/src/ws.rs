use std::{
    collections::HashSet,
    ops::ControlFlow,
    sync::{Arc, Mutex},
    time::Duration,
};

use api_model::buck2::ws::WSMessage;
use futures_util::{SinkExt, StreamExt};
use once_cell::sync::Lazy;
use tokio::{
    net::TcpStream,
    sync::{
        Semaphore, mpsc,
        mpsc::{UnboundedReceiver, UnboundedSender},
    },
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message,
};
use uuid::Uuid;

use crate::api::buck_build;

const DEFAULT_MAX_CONCURRENT_BUILDS: usize = 1;

static IN_FLIGHT_BUILD_IDS: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));
static BUILD_SLOTS: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let max_concurrent_builds = read_max_concurrent_builds();
    tracing::info!(
        "Build concurrency limit configured as {}",
        max_concurrent_builds
    );
    Arc::new(Semaphore::new(max_concurrent_builds))
});

struct InFlightBuildGuard {
    build_id: String,
}

impl InFlightBuildGuard {
    fn acquire(build_id: &str) -> Option<Self> {
        let inserted = with_inflight_build_ids(|inflight| inflight.insert(build_id.to_string()));
        if inserted {
            Some(Self {
                build_id: build_id.to_string(),
            })
        } else {
            None
        }
    }
}

impl Drop for InFlightBuildGuard {
    fn drop(&mut self) {
        with_inflight_build_ids(|inflight| {
            inflight.remove(&self.build_id);
        });
    }
}

fn with_inflight_build_ids<R>(f: impl FnOnce(&mut HashSet<String>) -> R) -> R {
    match IN_FLIGHT_BUILD_IDS.lock() {
        Ok(mut inflight) => f(&mut inflight),
        Err(poisoned) => {
            tracing::error!("In-flight build set lock was poisoned. Recovering state.");
            let mut inflight = poisoned.into_inner();
            f(&mut inflight)
        }
    }
}

fn read_max_concurrent_builds() -> usize {
    let Ok(value) = std::env::var("ORION_MAX_CONCURRENT_BUILDS") else {
        return DEFAULT_MAX_CONCURRENT_BUILDS;
    };

    match value.parse::<usize>() {
        Ok(parsed) if parsed > 0 => parsed,
        Ok(_) => {
            tracing::warn!(
                "ORION_MAX_CONCURRENT_BUILDS must be greater than 0. Falling back to {}.",
                DEFAULT_MAX_CONCURRENT_BUILDS
            );
            DEFAULT_MAX_CONCURRENT_BUILDS
        }
        Err(err) => {
            tracing::warn!(
                "Failed to parse ORION_MAX_CONCURRENT_BUILDS={}: {}. Falling back to {}.",
                value,
                err,
                DEFAULT_MAX_CONCURRENT_BUILDS
            );
            DEFAULT_MAX_CONCURRENT_BUILDS
        }
    }
}

/// Manages persistent WebSocket connection with automatic reconnection.
///
/// Handles connection establishment, registration, heartbeat, and task processing.
/// Implements exponential backoff for reconnection attempts.
///
/// # Arguments
/// * `server_addr` - WebSocket server endpoint URL
/// * `worker_id` - Unique identifier for this worker instance
pub async fn run_client(server_addr: String, worker_id: String) {
    let mut reconnect_delay = Duration::from_secs(1);
    const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

    loop {
        tracing::info!("Attempting to connect to server: {}", server_addr);
        match connect_async(&server_addr).await {
            Ok((ws_stream, response)) => {
                tracing::info!(
                    "WebSocket handshake successful. Server response: {:?}",
                    response.status()
                );
                // Reset reconnect delay after successful connection
                reconnect_delay = Duration::from_secs(1);
                // Handle the active connection
                handle_connection(ws_stream, worker_id.clone(), server_addr.clone()).await;
                tracing::warn!("Disconnected from server.");
            }
            Err(e) => {
                tracing::error!(
                    "WebSocket handshake failed: {}. Retrying in {:?}...",
                    e,
                    reconnect_delay
                );
            }
        }
        // Wait before attempting to reconnect
        tokio::time::sleep(reconnect_delay).await;
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
}

/// Processes an established WebSocket connection.
///
/// Coordinates three concurrent tasks:
/// - Heartbeat transmission to maintain connection
/// - Message sending from internal channels
/// - Message receiving and processing from server
///
/// # Arguments
/// * `ws_stream` - Established WebSocket connection
/// * `worker_id` - Worker identifier for registration
async fn handle_connection(
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    worker_id: String,
    server_addr: String,
) {
    let (ws_sender, mut ws_receiver) = ws_stream.split();
    let (internal_tx, mut internal_rx): (UnboundedSender<WSMessage>, UnboundedReceiver<WSMessage>) =
        mpsc::unbounded_channel();

    let worker_id_clone = worker_id.clone();
    let hostname_clone = server_addr.clone();
    let orion_version = env!("CARGO_PKG_VERSION").to_string(); // Get from Cargo.toml

    let internal_tx_clone = internal_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
        tracing::info!("Registering with worker ID: {}", worker_id_clone);
        if internal_tx_clone
            .send(WSMessage::Register {
                id: worker_id_clone,
                hostname: hostname_clone,
                orion_version,
            })
            .is_err()
        {
            tracing::error!("Failed to queue register message. Internal channel closed.");
            return;
        }
        let heartbeat_interval = Duration::from_secs(30);
        loop {
            tokio::time::sleep(heartbeat_interval).await;
            tracing::debug!("Sending heartbeat...");
            if internal_tx_clone.send(WSMessage::Heartbeat).is_err() {
                tracing::warn!("Failed to queue heartbeat message. Internal channel closed.");
                break;
            }
        }
    });

    let mut ws_sender = ws_sender;
    let send_task = tokio::spawn(async move {
        while let Some(msg) = internal_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(msg_str) => {
                    if let Err(e) = ws_sender.send(Message::Text(msg_str.into())).await {
                        tracing::error!(
                            "Failed to send message to server: {}. Terminating send task.",
                            e
                        );
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to serialize WSMessage: {}", e);
                }
            }
        }
    });

    let internal_tx_clone = internal_tx.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            if process_server_message(msg, internal_tx_clone.clone())
                .await
                .is_break()
            {
                break;
            }
        }
    });

    // Wait for any task to complete
    tokio::select! {
        _ = heartbeat_task => tracing::info!("Heartbeat task finished."),
        _ = send_task => tracing::info!("Send task finished."),
        _ = recv_task => tracing::info!("Receive task finished."),
    }
}

/// Processes incoming server messages and handles task execution.
///
/// Handles different message types including Task assignments and connection management.
/// For Task messages, performs in-flight dedupe and queues execution with bounded concurrency.
///
/// # Arguments
/// * `msg` - WebSocket message received from server
/// * `tx` - Channel for sending response messages
///
/// # Returns
/// * `ControlFlow::Continue(())` - Continue message processing
/// * `ControlFlow::Break(())` - Terminate connection
async fn process_server_message(
    msg: Message,
    sender: UnboundedSender<WSMessage>,
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => match serde_json::from_str::<WSMessage>(&t) {
            Ok(ws_msg) => {
                tracing::info!("Received message from server: {:?}", ws_msg);
                match ws_msg {
                    WSMessage::TaskBuild {
                        build_id,
                        repo,
                        cl_link,
                        changes,
                    } => {
                        tracing::info!("Received task: id={}", build_id);

                        let task_id_uuid = match Uuid::parse_str(&build_id) {
                            Ok(uuid) => uuid,
                            Err(e) => {
                                tracing::error!(
                                    "Failed to parse task id {} as Uuid: {}. Aborting task.",
                                    build_id,
                                    e
                                );
                                if let Err(send_err) = sender.send(WSMessage::TaskAck {
                                    build_id,
                                    success: false,
                                    message: format!("Invalid task id: {e}"),
                                }) {
                                    tracing::error!("Failed to send TaskAck: {}", send_err);
                                }
                                return ControlFlow::Continue(());
                            }
                        };

                        let Some(in_flight_guard) = InFlightBuildGuard::acquire(&build_id) else {
                            tracing::warn!(
                                "[Task {}] Duplicate build request ignored because this build is already in-flight.",
                                build_id
                            );
                            if let Err(e) = sender.send(WSMessage::TaskAck {
                                build_id,
                                success: true,
                                message:
                                    "Build task is already in progress; duplicate request ignored."
                                        .to_string(),
                            }) {
                                tracing::error!("Failed to send TaskAck: {}", e);
                            }
                            return ControlFlow::Continue(());
                        };

                        let sender_clone = sender.clone();
                        tokio::spawn(async move {
                            let _in_flight_guard = in_flight_guard;

                            if let Err(e) = sender_clone.send(WSMessage::TaskAck {
                                build_id: build_id.clone(),
                                success: true,
                                message: "Build task has been accepted and queued.".to_string(),
                            }) {
                                tracing::error!("Failed to send TaskAck: {}", e);
                            }

                            let _permit = match BUILD_SLOTS.clone().acquire_owned().await {
                                Ok(permit) => permit,
                                Err(e) => {
                                    let message = format!("Build queue is unavailable: {e}");
                                    tracing::error!("[Task {}] {}", build_id, message);
                                    if let Err(send_err) =
                                        sender_clone.send(WSMessage::TaskBuildComplete {
                                            build_id,
                                            success: false,
                                            exit_code: None,
                                            message,
                                            outcome: None,
                                        })
                                    {
                                        tracing::error!(
                                            "Failed to send TaskBuildComplete: {}",
                                            send_err
                                        );
                                    }
                                    return;
                                }
                            };

                            tracing::info!("[Task {}] Starting build after queue wait.", build_id);
                            let build_result = buck_build(
                                task_id_uuid,
                                cl_link,
                                repo,
                                changes,
                                sender_clone.clone(),
                            )
                            .await;

                            tracing::info!(
                                "[Task {}] Build finished. success={} exit_code={:?}",
                                build_result.build_id,
                                build_result.success,
                                build_result.exit_code
                            );
                        });
                    }
                    // Log unexpected message types
                    _ => {
                        tracing::warn!("Received unexpected message from server: {:?}", ws_msg);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Error deserializing message from server: {}", e);
            }
        },
        Message::Close(c) => {
            tracing::warn!("Server sent close frame: {:?}", c);
            return ControlFlow::Break(());
        }
        _ => {} // Ignore Binary, Ping, Pong and other message types
    }
    ControlFlow::Continue(())
}

#[cfg(test)]
mod tests {
    use super::InFlightBuildGuard;

    #[test]
    fn duplicate_build_id_is_rejected_while_guard_is_alive() {
        let first = InFlightBuildGuard::acquire("build-1");
        assert!(first.is_some());

        let second = InFlightBuildGuard::acquire("build-1");
        assert!(second.is_none());

        drop(first);

        let third = InFlightBuildGuard::acquire("build-1");
        assert!(third.is_some());
    }
}
