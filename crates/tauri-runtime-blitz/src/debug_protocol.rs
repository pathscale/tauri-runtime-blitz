//! Typed local debugging protocol carried by endpoint-libs framing.
//!
//! This is deliberately not WebDriver. It models the native renderer and app
//! lifecycle directly, supports observation by more than one client, and has
//! no authentication handshake. Agent control binds only to a local transport;
//! expensive diagnostics collection remains an explicit compile-time feature.

use endpoint_libs::libs::ws::{
    WireMessage,
    mcp_wire::{
        INVALID_PARAMS, INVALID_REQUEST, JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcRequest,
        JsonRpcResponse, MCP_PROTOCOL_VERSION, parse,
    },
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const DEBUG_PROTOCOL_VERSION: u32 = 1;
pub const MCP_INITIALIZE: &str = "initialize";
pub const MCP_INITIALIZED_NOTIFICATION: &str = "notifications/initialized";
pub const MCP_TOOLS_LIST: &str = "tools/list";
pub const MCP_TOOLS_CALL: &str = "tools/call";
pub const AGENT_CONTROL_TOOL: &str = "blitz.agent.control";
pub const AGENT_EVENT_NOTIFICATION: &str = "notifications/blitz/agent";
#[cfg(feature = "diagnostics")]
pub const DIAGNOSTICS_TOOL: &str = "blitz.diagnostics";
#[cfg(feature = "diagnostics")]
pub const DIAGNOSTICS_EVENT_NOTIFICATION: &str = "notifications/blitz/diagnostics";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugDescriptor {
    pub protocol_version: u32,
    pub pid: u32,
    pub instance_id: String,
    pub address: String,
    pub renderer: String,
    pub renderer_revision: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IncomingRequest {
    Initialize {
        id: JsonRpcId,
    },
    Initialized,
    ToolsList {
        id: JsonRpcId,
    },
    Agent {
        id: JsonRpcId,
        request: AgentControlRequest,
    },
    #[cfg(feature = "diagnostics")]
    Diagnostics {
        id: JsonRpcId,
        request: DiagnosticsRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "params", rename_all = "camelCase")]
pub enum AgentControlRequest {
    Inspect { root: Option<u64>, max_depth: u32 },
    Act(AgentAction),
    Relaunch,
    Quit,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "params", rename_all = "camelCase")]
pub enum DiagnosticsRequest {
    Observe { streams: Vec<DebugStream> },
    Snapshot(SnapshotRequest),
    Metrics,
    WaitForIdle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "camelCase")]
pub enum DebugResponse {
    Ack,
    AgentSnapshot(AgentSnapshot),
    #[cfg(feature = "diagnostics")]
    Snapshot(DebugSnapshot),
    #[cfg(feature = "diagnostics")]
    Metrics(RendererMetrics),
    #[cfg(feature = "diagnostics")]
    Idle(RevisionSet),
    Error(DebugError),
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "value", rename_all = "camelCase")]
pub enum DebugEvent {
    Snapshot(DebugSnapshot),
    Metrics(RendererMetrics),
    Console(ConsoleEntry),
    RuntimeError(RuntimeErrorEntry),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "value", rename_all = "camelCase")]
pub enum AgentControlEvent {
    Ready(DebugDescriptor),
    TreeChanged { revision: u64 },
    Lifecycle(LifecycleEvent),
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DebugStream {
    Snapshots,
    Metrics,
    Console,
    RuntimeErrors,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRequest {
    pub include_dom: bool,
    pub include_layout: bool,
    pub include_computed_style: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "params", rename_all = "camelCase")]
pub enum AgentAction {
    Click { node_id: u64 },
    SetValue { node_id: u64, value: String },
    ScrollIntoView { node_id: u64 },
    Input(InputCommand),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "camelCase")]
pub enum InputCommand {
    Key {
        phase: KeyPhase,
        key: String,
        code: String,
        modifiers: Modifiers,
    },
    Pointer {
        phase: PointerPhase,
        x: f64,
        y: f64,
        button: u16,
        modifiers: Modifiers,
    },
    Wheel {
        delta_x: f64,
        delta_y: f64,
        phase: WheelPhase,
        modifiers: Modifiers,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyPhase {
    Down,
    Up,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PointerPhase {
    Move,
    Down,
    Up,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WheelPhase {
    Started,
    Moved,
    Ended,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionSet {
    pub document: u64,
    pub style: u64,
    pub layout: u64,
    pub paint: u64,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugSnapshot {
    pub revisions: RevisionSet,
    pub active_window: Option<String>,
    pub active_element: Option<u64>,
    pub dom: Option<serde_json::Value>,
    pub layout: Option<serde_json::Value>,
    pub metrics: RendererMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSnapshot {
    pub revision: u64,
    pub active_window: Option<String>,
    pub focused_node: Option<u64>,
    pub nodes: Vec<SemanticNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticNode {
    pub id: u64,
    pub parent: Option<u64>,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub enabled: bool,
    pub visible: bool,
    pub selected: bool,
    pub bounds: Option<[f64; 4]>,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererMetrics {
    pub revisions: RevisionSet,
    pub queue_depth: u64,
    pub invalidations_coalesced: u64,
    pub frame: Option<FrameMetrics>,
    pub resident_bytes: Option<u64>,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameMetrics {
    pub input_to_present_ms: f64,
    pub style_ms: f64,
    pub layout_ms: f64,
    pub scene_ms: f64,
    pub submit_ms: f64,
    pub present_ms: f64,
    pub total_ms: f64,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    pub sequence: u64,
    pub level: String,
    pub message: String,
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeErrorEntry {
    pub sequence: u64,
    pub message: String,
    pub stack: String,
    pub revisions: RevisionSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum LifecycleEvent {
    Relaunching { replacement_instance_id: String },
    Ready { instance_id: String },
    Quitting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallParams {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallResult {
    content: Vec<TextContent>,
    structured_content: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TextContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

#[derive(Debug)]
pub enum DebugProtocolError {
    Json(serde_json::Error),
    Rpc(JsonRpcError),
    NonTextFrame,
    RequestIdRequired,
    UnexpectedMessage,
    UnexpectedMethod(String),
    UnexpectedTool(String),
}

impl std::fmt::Display for DebugProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid debug frame: {error}"),
            Self::Rpc(error) => write!(
                formatter,
                "JSON-RPC error {}: {}",
                error.code, error.message
            ),
            Self::NonTextFrame => formatter.write_str("debug protocol requires a text frame"),
            Self::RequestIdRequired => formatter.write_str("tools/call requires a request id"),
            Self::UnexpectedMessage => formatter.write_str("unexpected JSON-RPC message"),
            Self::UnexpectedMethod(method) => write!(formatter, "unexpected method {method}"),
            Self::UnexpectedTool(tool) => write!(formatter, "unexpected tool {tool}"),
        }
    }
}

impl std::error::Error for DebugProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for DebugProtocolError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn encode_agent_request(
    id: JsonRpcId,
    request: &AgentControlRequest,
) -> Result<WireMessage, DebugProtocolError> {
    encode_tool_request(id, AGENT_CONTROL_TOOL, request)
}

pub fn decode_incoming(message: WireMessage) -> Result<IncomingRequest, DebugProtocolError> {
    let JsonRpcMessage::Request(request) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    match request.method.as_str() {
        MCP_INITIALIZE => Ok(IncomingRequest::Initialize {
            id: request.id.ok_or(DebugProtocolError::RequestIdRequired)?,
        }),
        MCP_INITIALIZED_NOTIFICATION if request.is_notification() => {
            Ok(IncomingRequest::Initialized)
        }
        MCP_TOOLS_LIST => Ok(IncomingRequest::ToolsList {
            id: request.id.ok_or(DebugProtocolError::RequestIdRequired)?,
        }),
        MCP_TOOLS_CALL => decode_incoming_tool_call(request),
        _ => Err(DebugProtocolError::UnexpectedMethod(request.method)),
    }
}

pub fn encode_initialize_response(
    id: JsonRpcId,
    server_version: &str,
) -> Result<WireMessage, DebugProtocolError> {
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::result(
        Some(id),
        serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "tauri-runtime-blitz", "version": server_version}
        }),
    )))
}

pub fn encode_tools_list_response(id: JsonRpcId) -> Result<WireMessage, DebugProtocolError> {
    #[allow(unused_mut)]
    let mut tools = vec![serde_json::json!({
        "name": AGENT_CONTROL_TOOL,
        "description": "Inspect and operate the native Blitz semantic UI tree",
        "inputSchema": {"type": "object"}
    })];
    #[cfg(feature = "diagnostics")]
    tools.push(serde_json::json!({
        "name": DIAGNOSTICS_TOOL,
        "description": "Observe Blitz DOM, layout, errors, and renderer metrics",
        "inputSchema": {"type": "object"}
    }));
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::result(
        Some(id),
        serde_json::json!({"tools": tools}),
    )))
}

pub fn encode_rpc_error(
    id: Option<JsonRpcId>,
    error: JsonRpcError,
) -> Result<WireMessage, DebugProtocolError> {
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::error(id, error)))
}

pub fn decode_agent_request(
    message: WireMessage,
) -> Result<(JsonRpcId, AgentControlRequest), DebugProtocolError> {
    decode_tool_request(message, AGENT_CONTROL_TOOL)
}

#[cfg(feature = "diagnostics")]
pub fn encode_diagnostics_request(
    id: JsonRpcId,
    request: &DiagnosticsRequest,
) -> Result<WireMessage, DebugProtocolError> {
    encode_tool_request(id, DIAGNOSTICS_TOOL, request)
}

#[cfg(feature = "diagnostics")]
pub fn decode_diagnostics_request(
    message: WireMessage,
) -> Result<(JsonRpcId, DiagnosticsRequest), DebugProtocolError> {
    decode_tool_request(message, DIAGNOSTICS_TOOL)
}

pub fn encode_response(
    id: JsonRpcId,
    response: &DebugResponse,
) -> Result<WireMessage, DebugProtocolError> {
    let structured_content = serde_json::to_value(response)?;
    let result = ToolCallResult {
        content: vec![TextContent {
            content_type: "text".into(),
            // structuredContent is the typed payload consumed by the local
            // client. Repeating a full DOM snapshot as text doubled the frame
            // and could cross endpoint-libs' 16 MiB safety limit, which closed
            // an otherwise healthy socket. Keep the required human-readable
            // MCP content concise and send the snapshot exactly once.
            text: response_summary(response),
        }],
        structured_content,
        is_error: matches!(response, DebugResponse::Error(_)),
    };
    encode_rpc(JsonRpcMessage::Response(JsonRpcResponse::result(
        Some(id),
        serde_json::to_value(result)?,
    )))
}

fn response_summary(response: &DebugResponse) -> String {
    match response {
        DebugResponse::Ack => "ok".into(),
        DebugResponse::AgentSnapshot(snapshot) => {
            format!("semantic snapshot with {} nodes", snapshot.nodes.len())
        }
        #[cfg(feature = "diagnostics")]
        DebugResponse::Snapshot(_) => "diagnostic snapshot".into(),
        #[cfg(feature = "diagnostics")]
        DebugResponse::Metrics(_) => "renderer metrics".into(),
        #[cfg(feature = "diagnostics")]
        DebugResponse::Idle(_) => "renderer idle".into(),
        DebugResponse::Error(error) => error.message.clone(),
    }
}

pub fn decode_response(
    message: WireMessage,
) -> Result<(JsonRpcId, DebugResponse), DebugProtocolError> {
    let JsonRpcMessage::Response(response) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    let id = response.id.ok_or(DebugProtocolError::RequestIdRequired)?;
    if let Some(error) = response.error {
        return Err(DebugProtocolError::Rpc(error));
    }
    let result: ToolCallResult = serde_json::from_value(
        response
            .result
            .ok_or(DebugProtocolError::UnexpectedMessage)?,
    )?;
    Ok((id, serde_json::from_value(result.structured_content)?))
}

pub fn encode_agent_event(event: &AgentControlEvent) -> Result<WireMessage, DebugProtocolError> {
    encode_notification(AGENT_EVENT_NOTIFICATION, event)
}

pub fn decode_agent_event(message: WireMessage) -> Result<AgentControlEvent, DebugProtocolError> {
    decode_notification(message, AGENT_EVENT_NOTIFICATION)
}

#[cfg(feature = "diagnostics")]
pub fn encode_diagnostics_event(event: &DebugEvent) -> Result<WireMessage, DebugProtocolError> {
    encode_notification(DIAGNOSTICS_EVENT_NOTIFICATION, event)
}

#[cfg(feature = "diagnostics")]
pub fn decode_diagnostics_event(message: WireMessage) -> Result<DebugEvent, DebugProtocolError> {
    decode_notification(message, DIAGNOSTICS_EVENT_NOTIFICATION)
}

fn encode_tool_request<T: Serialize>(
    id: JsonRpcId,
    tool: &str,
    request: &T,
) -> Result<WireMessage, DebugProtocolError> {
    let params = ToolCallParams {
        name: tool.into(),
        arguments: serde_json::to_value(request)?,
    };
    encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
        id,
        MCP_TOOLS_CALL,
        serde_json::to_value(params)?,
    )))
}

fn decode_tool_request<T: DeserializeOwned>(
    message: WireMessage,
    expected_tool: &str,
) -> Result<(JsonRpcId, T), DebugProtocolError> {
    let JsonRpcMessage::Request(request) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    if request.method != MCP_TOOLS_CALL {
        return Err(DebugProtocolError::UnexpectedMethod(request.method));
    }
    let id = request.id.ok_or(DebugProtocolError::RequestIdRequired)?;
    let params: ToolCallParams = serde_json::from_value(request.params.ok_or_else(|| {
        DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_PARAMS,
            "missing tools/call params",
        ))
    })?)?;
    if params.name != expected_tool {
        return Err(DebugProtocolError::UnexpectedTool(params.name));
    }
    Ok((id, serde_json::from_value(params.arguments)?))
}

fn decode_incoming_tool_call(
    request: JsonRpcRequest,
) -> Result<IncomingRequest, DebugProtocolError> {
    let id = request.id.ok_or(DebugProtocolError::RequestIdRequired)?;
    let params: ToolCallParams = serde_json::from_value(request.params.ok_or_else(|| {
        DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_PARAMS,
            "missing tools/call params",
        ))
    })?)?;
    match params.name.as_str() {
        AGENT_CONTROL_TOOL => Ok(IncomingRequest::Agent {
            id,
            request: serde_json::from_value(params.arguments)?,
        }),
        #[cfg(feature = "diagnostics")]
        DIAGNOSTICS_TOOL => Ok(IncomingRequest::Diagnostics {
            id,
            request: serde_json::from_value(params.arguments)?,
        }),
        _ => Err(DebugProtocolError::UnexpectedTool(params.name)),
    }
}

fn encode_notification<T: Serialize>(
    method: &str,
    value: &T,
) -> Result<WireMessage, DebugProtocolError> {
    encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::notification(
        method,
        serde_json::to_value(value)?,
    )))
}

fn decode_notification<T: DeserializeOwned>(
    message: WireMessage,
    expected_method: &str,
) -> Result<T, DebugProtocolError> {
    let JsonRpcMessage::Request(request) = decode_rpc(message)? else {
        return Err(DebugProtocolError::UnexpectedMessage);
    };
    if !request.is_notification() {
        return Err(DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_REQUEST,
            "notification must not contain an id",
        )));
    }
    if request.method != expected_method {
        return Err(DebugProtocolError::UnexpectedMethod(request.method));
    }
    serde_json::from_value(request.params.ok_or_else(|| {
        DebugProtocolError::Rpc(JsonRpcError::new(
            INVALID_PARAMS,
            "missing notification params",
        ))
    })?)
    .map_err(Into::into)
}

pub(crate) fn encode_rpc(message: JsonRpcMessage) -> Result<WireMessage, DebugProtocolError> {
    Ok(WireMessage::Text(serde_json::to_string(&message)?))
}

pub(crate) fn decode_rpc(message: WireMessage) -> Result<JsonRpcMessage, DebugProtocolError> {
    let WireMessage::Text(text) = message else {
        return Err(DebugProtocolError::NonTextFrame);
    };
    parse(&text).map_err(DebugProtocolError::Rpc)
}

#[cfg(test)]
mod tests {
    use endpoint_libs::libs::ws::MessageStream;
    use endpoint_libs::libs::ws::transport::{TransportStream, framed_json};

    use super::*;

    fn key_request() -> AgentControlRequest {
        AgentControlRequest::Act(AgentAction::Input(InputCommand::Key {
            phase: KeyPhase::Down,
            key: "2".into(),
            code: "Digit2".into(),
            modifiers: Modifiers {
                meta: true,
                ..Default::default()
            },
        }))
    }

    #[test]
    fn typed_modifier_input_round_trips_mcp_tools_call() {
        let request = key_request();
        let id = JsonRpcId::Number(7);
        assert_eq!(
            decode_agent_request(encode_agent_request(id.clone(), &request).unwrap()).unwrap(),
            (id, request)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn endpoint_framing_carries_debug_frames_without_a_session_or_token() {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let mut server = TransportStream::new(framed_json(server_io));
        let mut client = TransportStream::new(framed_json(client_io));
        let request = key_request();
        let id = JsonRpcId::String("input-7".into());

        client
            .send(encode_agent_request(id.clone(), &request).unwrap())
            .await
            .unwrap();
        let received = server.recv().await.unwrap().unwrap();

        assert_eq!(decode_agent_request(received).unwrap(), (id, request));
    }

    #[test]
    fn unrelated_mcp_tool_is_rejected_explicitly() {
        let message = JsonRpcMessage::Request(JsonRpcRequest::call(
            JsonRpcId::Number(1),
            MCP_TOOLS_CALL,
            serde_json::json!({"name": "other.tool", "arguments": {}}),
        ));
        assert!(matches!(
            decode_agent_request(encode_rpc(message).unwrap()),
            Err(DebugProtocolError::UnexpectedTool(tool)) if tool == "other.tool"
        ));
    }

    #[test]
    fn responses_and_notifications_round_trip() {
        let id = JsonRpcId::Number(19);
        let response = DebugResponse::Ack;
        assert_eq!(
            decode_response(encode_response(id.clone(), &response).unwrap()).unwrap(),
            (id, response)
        );

        let event = AgentControlEvent::TreeChanged { revision: 12 };
        assert_eq!(
            decode_agent_event(encode_agent_event(&event).unwrap()).unwrap(),
            event
        );
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn diagnostics_metrics_request_and_response_round_trip() {
        let id = JsonRpcId::Number(23);
        assert_eq!(
            decode_diagnostics_request(
                encode_diagnostics_request(id.clone(), &DiagnosticsRequest::Metrics).unwrap()
            )
            .unwrap(),
            (id.clone(), DiagnosticsRequest::Metrics)
        );

        let response = DebugResponse::Metrics(RendererMetrics {
            resident_bytes: Some(4096),
            ..Default::default()
        });
        assert_eq!(
            decode_response(encode_response(id.clone(), &response).unwrap()).unwrap(),
            (id, response)
        );
    }
}
