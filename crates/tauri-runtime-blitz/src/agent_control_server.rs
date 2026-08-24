use std::fs::{OpenOptions, remove_file};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use endpoint_libs::libs::ws::MessageStream;
#[cfg(test)]
use endpoint_libs::libs::ws::WireMessage;
use endpoint_libs::libs::ws::mcp_wire::{INVALID_REQUEST, JsonRpcError};
use endpoint_libs::libs::ws::transport::{TransportStream, framed_json};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

#[cfg(feature = "diagnostics")]
use crate::control_protocol::DiagnosticsRequest;
use crate::control_protocol::{
    AgentControlRequest, DebugDescriptor, DebugResponse, IncomingRequest, decode_incoming,
    encode_initialize_response, encode_response, encode_rpc_error, encode_tools_list_response,
    peek_request_id,
};

pub(crate) enum ControlBridgeRequest {
    Agent(AgentControlRequest),
    #[cfg(feature = "diagnostics")]
    Diagnostics(DiagnosticsRequest),
}

pub(crate) type ControlBridge =
    Arc<dyn Fn(ControlBridgeRequest) -> oneshot::Receiver<DebugResponse> + Send + Sync + 'static>;

#[cfg(test)]
pub(crate) static CONTROL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) struct AgentControlServer {
    descriptor_path: PathBuf,
    socket_path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// Holds deep-profiling collection open while a tool can attach.
    ///
    /// The consumer is out of process, so it cannot hold a session itself and
    /// the server holds one on its behalf. The server's lifetime is the right
    /// one: it exists exactly while the socket is listening, and a per-request
    /// session would be useless, because the frames a snapshot reports were
    /// presented before the request arrived.
    ///
    /// `None` when the profile does not permit sampling, which is the ordinary
    /// case: inspection without deep profiling stays free.
    _sampling: Option<blitz_shell::DeepProfilingSession>,
}

impl AgentControlServer {
    pub(crate) fn start(bridge: ControlBridge) -> io::Result<Self> {
        let instance_id = instance_id();
        let descriptor_path = descriptor_path(&instance_id);
        let socket_path = descriptor_path.with_extension("sock");
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = remove_file(&socket_path);

        let listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let descriptor = DebugDescriptor {
            protocol_version: crate::control_protocol::DEBUG_PROTOCOL_VERSION,
            pid: std::process::id(),
            instance_id,
            address: format!("unix://{}", socket_path.display()),
            renderer: "blitz".into(),
            renderer_revision: env!("CARGO_PKG_VERSION").into(),
        };
        write_descriptor(&descriptor_path, &descriptor)?;
        reap_dead_descriptors(&descriptor_path);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("blitz-agent-control".into())
            .spawn(move || run(listener, bridge, shutdown_rx))?;

        Ok(Self {
            descriptor_path,
            socket_path,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
            // Taken after the socket is listening, so a permitted profile
            // begins collecting for the tool that is now able to attach, and
            // stops again when this server is dropped.
            _sampling: blitz_shell::begin_deep_profiling(),
        })
    }

    #[cfg(test)]
    fn descriptor_path(&self) -> &Path {
        &self.descriptor_path
    }

    #[cfg(test)]
    fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for AgentControlServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = remove_file(&self.socket_path);
        let _ = remove_file(&self.descriptor_path);
    }
}

fn run(
    listener: std::os::unix::net::UnixListener,
    bridge: ControlBridge,
    shutdown: oneshot::Receiver<()>,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let Ok(listener) = UnixListener::from_std(listener) else {
            return;
        };
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let bridge = Arc::clone(&bridge);
                        tokio::task::spawn_local(async move {
                            handle_connection(stream, bridge).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    });
}

async fn handle_connection(stream: UnixStream, bridge: ControlBridge) {
    let mut stream = TransportStream::new(framed_json(stream));
    while let Some(message) = stream.recv().await {
        let response = match message {
            Ok(message) => {
                // Recovered before the typed decode consumes the frame, so a
                // malformed request is answered to the caller rather than to
                // nobody. See peek_request_id.
                let request_id = peek_request_id(&message);
                match decode_incoming(message) {
                    Ok(IncomingRequest::Initialize { id }) => {
                        encode_initialize_response(id, env!("CARGO_PKG_VERSION"))
                    }
                    Ok(IncomingRequest::Initialized) => continue,
                    Ok(IncomingRequest::ToolsList { id }) => {
                        encode_tools_list_response(id, cfg!(feature = "diagnostics"))
                    }
                    Ok(IncomingRequest::Agent { id, request }) => {
                        let response = bridge(ControlBridgeRequest::Agent(request))
                            .await
                            .unwrap_or_else(|_| {
                                DebugResponse::Error(crate::control_protocol::DebugError {
                                    code: "bridgeClosed".into(),
                                    message: "the UI-thread control bridge closed".into(),
                                })
                            });
                        encode_response(id, &response)
                    }
                    // The protocol defines diagnostics unconditionally; only
                    // collection is feature-gated. A build without it answers
                    // the caller instead of failing to compile the arm, which
                    // is the whole reason the types are not gated.
                    Ok(IncomingRequest::Diagnostics {
                        id,
                        request: _request,
                    }) => {
                        #[cfg(feature = "diagnostics")]
                        let response = bridge(ControlBridgeRequest::Diagnostics(_request))
                            .await
                            .unwrap_or_else(|_| {
                                DebugResponse::Error(crate::control_protocol::DebugError {
                                    code: "bridgeClosed".into(),
                                    message: "the UI-thread diagnostics bridge closed".into(),
                                })
                            });
                        #[cfg(not(feature = "diagnostics"))]
                        let response = DebugResponse::Error(crate::control_protocol::DebugError {
                            code: "diagnosticsUnavailable".into(),
                            message: "this build has no diagnostics feature; \
                                      rebuild with tauri-runtime-blitz/diagnostics"
                                .into(),
                        });
                        encode_response(id, &response)
                    }
                    Err(error) => encode_rpc_error(
                        request_id,
                        JsonRpcError::new(INVALID_REQUEST, error.to_string()),
                    ),
                }
            }
            // A transport error is not a bad request: the framing is broken or
            // the peer is gone, and the next read returns the same error
            // immediately. Answering and continuing spun this task at a full
            // core for the life of the process, one per client that ever
            // disconnected, which is most of them. Measured on an idle app:
            // 0.0% CPU without the control server, 55-76% with it after a few
            // tools had connected and gone.
            //
            // So answer once, best effort, then stop reading this connection.
            Err(error) => {
                let farewell =
                    encode_rpc_error(None, JsonRpcError::new(INVALID_REQUEST, error.to_string()));
                if let Ok(farewell) = farewell {
                    let _ = stream.send(farewell).await;
                }
                break;
            }
        };
        let response = response.unwrap_or_else(|error| {
            encode_rpc_error(
                None,
                JsonRpcError::new(
                    INVALID_REQUEST,
                    format!("could not encode response: {error}"),
                ),
            )
            .expect("the fallback JSON-RPC error is serializable")
        });
        if stream.send(response).await.is_err() {
            break;
        }
    }
}

fn instance_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos:x}", std::process::id())
}

/// Whether a pid still names a live process.
///
/// `kill(pid, 0)` without a libc dependency, and `/proc` does not exist on
/// macOS. A `ps` that cannot be run at all reports "live", because deleting
/// another instance's descriptor on a bad guess is far worse than keeping a
/// stale file.
fn pid_is_live(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(true)
}

/// Delete descriptors whose process is gone.
///
/// The name carries a pid and a nanosecond stamp, so every launch leaves a new
/// pair behind and nothing ever removed them: a developer machine accumulates
/// them indefinitely, and one here had **99**. `Drop` cleans up an orderly
/// exit, but a crash, a `kill -9`, or a rebuild that unlinks the socket under a
/// running instance all skip it.
///
/// That is not merely untidy. A tool with no `TAURI_BLITZ_CONTROL_DESCRIPTOR`
/// has to guess which of them is current, and a directory full of dead entries
/// is what makes "attached to a stale socket and reported numbers for a process
/// nobody is looking at" a routine failure rather than a rare one.
///
/// Only entries whose pid is dead are removed, so concurrent instances are left
/// strictly alone — and this one's own descriptor is skipped by path, since it
/// has just been written and its pid is obviously live.
fn reap_dead_descriptors(own: &Path) {
    reap_dead_descriptors_with(own, pid_is_live);
}

fn reap_dead_descriptors_with(own: &Path, is_live: impl Fn(u32) -> bool) {
    let Some(dir) = own.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own || path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(descriptor) = serde_json::from_str::<DebugDescriptor>(&text) else {
            continue;
        };
        if is_live(descriptor.pid) {
            continue;
        }
        let _ = remove_file(path.with_extension("sock"));
        let _ = remove_file(&path);
    }
}

fn descriptor_path(instance_id: &str) -> PathBuf {
    std::env::var_os("TAURI_BLITZ_CONTROL_DESCRIPTOR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("tauri-blitz-agent")
                .join(format!("{instance_id}.json"))
        })
}

fn write_descriptor(path: &Path, descriptor: &DebugDescriptor) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, descriptor).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use endpoint_libs::libs::ws::mcp_wire::{JsonRpcId, JsonRpcMessage, JsonRpcRequest};

    use super::*;
    use crate::control_protocol::{
        AgentAction, AgentControlRequest, DebugResponse, MCP_INITIALIZE, MCP_TOOLS_LIST,
        decode_response, decode_rpc, encode_agent_request, encode_rpc,
    };
    #[cfg(feature = "diagnostics")]
    use crate::control_protocol::{
        DebugSnapshot, DiagnosticsRequest, RendererMetrics, RevisionSet, SnapshotRequest,
        encode_diagnostics_request,
    };

    #[tokio::test(flavor = "current_thread")]
    async fn local_server_is_mcp_compatible_and_needs_no_session_or_token() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|request| {
            let (sender, receiver) = oneshot::channel();
            assert!(matches!(
                request,
                ControlBridgeRequest::Agent(AgentControlRequest::Act(AgentAction::Click {
                    node_id: 42
                }))
            ));
            sender.send(DebugResponse::Ack).unwrap();
            receiver
        });
        let server = AgentControlServer::start(bridge).unwrap();
        assert!(server.descriptor_path().is_file());
        assert_eq!(
            server
                .descriptor_path()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));
        stream
            .send(
                encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
                    JsonRpcId::Number(1),
                    MCP_INITIALIZE,
                    serde_json::json!({"protocolVersion": "2025-06-18"}),
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            decode_rpc(stream.recv().await.unwrap().unwrap()).unwrap(),
            JsonRpcMessage::Response(_)
        ));
        stream
            .send(
                encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
                    JsonRpcId::Number(2),
                    MCP_TOOLS_LIST,
                    serde_json::json!({}),
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            decode_rpc(stream.recv().await.unwrap().unwrap()).unwrap(),
            JsonRpcMessage::Response(_)
        ));
        let id = JsonRpcId::Number(42);
        stream
            .send(
                encode_agent_request(
                    id.clone(),
                    &AgentControlRequest::Act(AgentAction::Click { node_id: 42 }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let response = stream.recv().await.unwrap().unwrap();
        assert_eq!(decode_response(response).unwrap(), (id, DebugResponse::Ack));

        let second = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut second = TransportStream::new(framed_json(second));
        let id = JsonRpcId::String("second-observer".into());
        second
            .send(
                encode_agent_request(
                    id.clone(),
                    &AgentControlRequest::Act(AgentAction::Click { node_id: 42 }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            decode_response(second.recv().await.unwrap().unwrap()).unwrap(),
            (id, DebugResponse::Ack)
        );

        drop(server);
    }

    #[cfg(feature = "diagnostics")]
    #[tokio::test(flavor = "current_thread")]
    async fn diagnostics_metrics_reach_the_runtime_bridge_over_mcp() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|request| {
            let (sender, receiver) = oneshot::channel();
            assert!(matches!(
                request,
                ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Metrics)
            ));
            sender
                .send(DebugResponse::Metrics(RendererMetrics {
                    resident_bytes: Some(8192),
                    ..Default::default()
                }))
                .unwrap();
            receiver
        });
        let server = AgentControlServer::start(bridge).unwrap();
        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));
        let id = JsonRpcId::Number(91);

        stream
            .send(encode_diagnostics_request(id.clone(), &DiagnosticsRequest::Metrics).unwrap())
            .await
            .unwrap();

        assert_eq!(
            decode_response(stream.recv().await.unwrap().unwrap()).unwrap(),
            (
                id,
                DebugResponse::Metrics(RendererMetrics {
                    resident_bytes: Some(8192),
                    ..Default::default()
                })
            )
        );
    }

    #[cfg(feature = "diagnostics")]
    #[tokio::test(flavor = "current_thread")]
    async fn large_snapshot_keeps_the_socket_open_for_follow_up_requests() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        const LARGE_DOM_BYTES: usize = 9 * 1024 * 1024;
        let bridge: ControlBridge = Arc::new(|request| {
            let (sender, receiver) = oneshot::channel();
            let response = match request {
                ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Snapshot(_)) => {
                    DebugResponse::Snapshot(DebugSnapshot {
                        revisions: RevisionSet::default(),
                        active_window: Some("main".into()),
                        active_element: None,
                        dom: Some(serde_json::Value::String("x".repeat(LARGE_DOM_BYTES))),
                        layout: None,
                        computed_style: None,
                        metrics: RendererMetrics::default(),
                    })
                }
                ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Metrics) => {
                    DebugResponse::Metrics(RendererMetrics {
                        resident_bytes: Some(4096),
                        ..Default::default()
                    })
                }
                _ => panic!("unexpected request"),
            };
            sender.send(response).unwrap();
            receiver
        });
        let server = AgentControlServer::start(bridge).unwrap();
        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));

        let snapshot_id = JsonRpcId::Number(92);
        stream
            .send(
                encode_diagnostics_request(
                    snapshot_id.clone(),
                    &DiagnosticsRequest::Snapshot(SnapshotRequest {
                        include_dom: true,
                        include_layout: false,
                        include_computed_style: false,
                    }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let (response_id, response) =
            decode_response(stream.recv().await.unwrap().unwrap()).unwrap();
        assert_eq!(response_id, snapshot_id);
        let DebugResponse::Snapshot(snapshot) = response else {
            panic!("expected a diagnostic snapshot")
        };
        assert_eq!(
            snapshot.dom.unwrap().as_str().unwrap().len(),
            LARGE_DOM_BYTES
        );

        let metrics_id = JsonRpcId::Number(93);
        stream
            .send(
                encode_diagnostics_request(metrics_id.clone(), &DiagnosticsRequest::Metrics)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            decode_response(stream.recv().await.unwrap().unwrap()).unwrap(),
            (
                metrics_id,
                DebugResponse::Metrics(RendererMetrics {
                    resident_bytes: Some(4096),
                    ..Default::default()
                })
            )
        );
    }

    #[test]
    fn initialize_payload_uses_json_rpc() {
        let message = encode_initialize_response(JsonRpcId::Number(1), "0.1.0").unwrap();
        let WireMessage::Text(payload) = message else {
            panic!("initialize response must be text")
        };
        let decoded: JsonRpcMessage = serde_json::from_str(&payload).unwrap();
        assert!(matches!(decoded, JsonRpcMessage::Response(_)));
    }

    /**
     * A dead instance's descriptor goes; a live one's stays.
     *
     * The filename carries a pid and a nanosecond stamp, so every launch leaves
     * a new pair behind and nothing removed them — one machine had 99. `Drop`
     * handles an orderly exit, but a crash, a `kill -9`, or a rebuild that
     * unlinks the socket under a running instance all skip it. A tool with no
     * `TAURI_BLITZ_CONTROL_DESCRIPTOR` then has to guess which is current,
     * which is how attaching to a stale socket became routine.
     *
     * The asymmetry is the whole point: reaping too eagerly would delete a
     * concurrent instance's descriptor, which is worse than leaving litter.
     */
    #[test]
    fn reaping_removes_dead_descriptors_and_spares_live_ones() {
        let dir = std::env::temp_dir().join(format!(
            "blitz-reap-{}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch directory is created");

        let write = |name: &str, pid: u32| {
            let path = dir.join(format!("{name}.json"));
            write_descriptor(
                &path,
                &DebugDescriptor {
                    protocol_version: crate::control_protocol::DEBUG_PROTOCOL_VERSION,
                    pid,
                    instance_id: name.into(),
                    address: format!("unix://{}", dir.join(format!("{name}.sock")).display()),
                    renderer: "blitz".into(),
                    renderer_revision: "0.0.0".into(),
                },
            )
            .expect("descriptor is written");
            std::fs::write(dir.join(format!("{name}.sock")), b"").expect("socket stub is written");
            path
        };

        // Liveness is injected here because process inspection may be denied by
        // the test sandbox. This test owns descriptor cleanup, not `ps` itself.
        let own = write("own", std::process::id());
        let live = write("live", 1);
        let dead = write("dead", 2);

        reap_dead_descriptors_with(&own, |pid| pid == std::process::id() || pid == 1);

        assert!(own.exists(), "the caller's own descriptor is never reaped");
        assert!(live.exists(), "a live instance must keep its descriptor");
        assert!(!dead.exists(), "a dead instance's descriptor is removed");
        assert!(
            !dir.join("dead.sock").exists(),
            "the orphaned socket goes with it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
