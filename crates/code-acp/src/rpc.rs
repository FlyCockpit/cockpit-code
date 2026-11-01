//! JSON-RPC plumbing: the single stdout writer and the outbound request
//! registry (`session/request_permission`, `elicitation/create`).
//!
//! The registry keeps Cockpit's bounds (64 live outbound requests) and its
//! closed-connection rule: every pending request resolves with an error when
//! the peer disconnects, so no engine approval or question waits forever.
//! Cockpit's daemon attention states are not needed: the engine owns the
//! approval and question records.

use crate::codec::prepare_outbound;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};

/// Live outbound requests at once (Cockpit `ACP_OUTBOUND_PERMISSION_MAX_ENTRIES_V1`).
pub const MAX_OUTBOUND: usize = 64;

pub const INVALID_PARAMS: i64 = -32602;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INTERNAL: i64 = -32603;
pub const NOT_FOUND: i64 = -32002;
/// `session/load` hit a held lock; `data` names the fork and claim options.
pub const SESSION_LOCKED: i64 = -32050;

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL, message)
    }
    fn closed() -> Self {
        Self::internal("ACP connection closed")
    }
}

pub type Reply = Result<Value, RpcError>;

pub struct Out {
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next: AtomicU64,
    closed: AtomicBool,
}

impl Out {
    pub fn new(tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            tx,
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    fn write(&self, value: Value) {
        let text = value.to_string();
        match prepare_outbound(&text) {
            Ok(()) => {
                let _ = self.tx.send(text);
            }
            Err(e) => eprintln!("acp: dropped outbound frame: {e}"),
        }
    }

    /// Unchecked send; `run` uses an empty string as the writer's stop sentinel.
    pub fn raw_send(&self, frame: String) -> Result<(), ()> {
        self.tx.send(frame).map_err(|_| ())
    }

    pub fn respond(&self, id: &Value, result: Value) {
        self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub fn fail(&self, id: &Value, error: RpcError) {
        let mut body = json!({"code": error.code, "message": error.message});
        if let Some(data) = error.data {
            body["data"] = data;
        }
        self.write(json!({"jsonrpc": "2.0", "id": id, "error": body}));
    }

    pub fn reply(&self, id: &Value, reply: Reply) {
        match reply {
            Ok(result) => self.respond(id, result),
            Err(error) => self.fail(id, error),
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// One `session/update` notification.
    pub fn update(&self, session_id: &str, update: Value) {
        let params = json!({"sessionId": session_id, "update": update});
        conforms::<agent_client_protocol_schema::v1::SessionNotification>(
            "session/update",
            &params,
        );
        self.notify("session/update", params);
    }

    /// Send a request to the client and wait for its response.
    pub async fn request(&self, method: &str, params: Value) -> Reply {
        if self.closed.load(Ordering::Acquire) {
            return Err(RpcError::closed());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.len() >= MAX_OUTBOUND {
                return Err(RpcError::internal("outbound_request_capacity_exhausted"));
            }
            pending.insert(id, tx);
        }
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await.unwrap_or_else(|_| Err(RpcError::closed()))
    }

    /// Route a response frame to its waiting request; unknown ids are ignored.
    pub fn on_response(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(tx) = self.pending.lock().unwrap().remove(&id) else {
            return;
        };
        let reply = match frame.get("error") {
            Some(error) => Err(RpcError {
                code: error["code"].as_i64().unwrap_or(INTERNAL),
                message: error["message"].as_str().unwrap_or("error").to_string(),
                data: error.get("data").cloned(),
            }),
            None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = tx.send(reply);
    }

    /// The peer is gone: fail every pending request and refuse new ones.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(RpcError::closed()));
        }
    }
}

/// Debug builds check outbound payloads against the pinned ACP schema types,
/// reporting mismatches on stderr (stdout carries protocol frames only).
pub fn conforms<T: DeserializeOwned>(what: &str, value: &Value) {
    if cfg!(debug_assertions)
        && let Err(e) = serde_json::from_value::<T>(value.clone())
    {
        eprintln!("acp: {what} does not match the ACP v1 schema: {e}");
    }
}

/// Decode request params with the pinned ACP schema type.
pub fn decode<T: DeserializeOwned>(params: &Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone()).map_err(|e| RpcError::params(e.to_string()))
}
