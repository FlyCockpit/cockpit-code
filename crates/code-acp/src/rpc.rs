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

    fn write(&self, value: Value) -> Result<(), RpcError> {
        let text = value.to_string();
        prepare_outbound(&text).map_err(|e| {
            eprintln!("acp: dropped outbound frame: {e}");
            RpcError::internal(format!("invalid outbound frame: {e}"))
        })?;
        self.tx.send(text).map_err(|_| RpcError::closed())
    }

    /// Unchecked send; `run` uses an empty string as the writer's stop sentinel.
    pub fn raw_send(&self, frame: String) -> Result<(), ()> {
        self.tx.send(frame).map_err(|_| ())
    }

    pub fn respond(&self, id: &Value, result: Value) {
        let _ = self.write(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub fn fail(&self, id: &Value, error: RpcError) {
        let mut body = json!({"code": error.code, "message": error.message});
        if let Some(data) = error.data {
            body["data"] = data;
        }
        let _ = self.write(json!({"jsonrpc": "2.0", "id": id, "error": body}));
    }

    pub fn reply(&self, id: &Value, reply: Reply) {
        match reply {
            Ok(result) => self.respond(id, result),
            Err(error) => self.fail(id, error),
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}));
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
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            // Registration and close's drain must share this lock, so a
            // request can never slip into the registry after it was drained.
            if self.closed.load(Ordering::Acquire) {
                return Err(RpcError::closed());
            }
            if pending.len() >= MAX_OUTBOUND {
                return Err(RpcError::internal("outbound_request_capacity_exhausted"));
            }
            pending.insert(id, tx);
        }
        if let Err(error) =
            self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
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
        let mut pending = self.pending.lock().unwrap();
        self.closed.store(true, Ordering::Release);
        for (_, tx) in pending.drain() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    use tokio::time::timeout;

    const DEADLINE: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn failed_channel_write_removes_the_pending_request() {
        let (tx, rx) = mpsc::unbounded_channel();
        let out = Out::new(tx);
        drop(rx);

        let error = timeout(DEADLINE, out.request("test", Value::Null))
            .await
            .expect("failed write must not hang")
            .unwrap_err();
        assert_eq!(error.message, "ACP connection closed");
        assert!(out.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_frame_removes_the_pending_request() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = Out::new(tx);
        let oversized = Value::String("x".repeat(crate::codec::MAX_FRAME_BYTES));

        let error = timeout(DEADLINE, out.request("test", oversized))
            .await
            .expect("rejected frame must not hang")
            .unwrap_err();
        assert!(error.message.contains("invalid outbound frame"));
        assert!(out.pending.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
        // A rejected request does not poison the connection.
        let request = out.request("test", Value::Null);
        let response = async {
            let frame: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
            out.on_response(&json!({"id": frame["id"], "result": "ok"}));
        };
        let (reply, ()) = timeout(DEADLINE, async { tokio::join!(request, response) })
            .await
            .expect("next valid request must complete");
        assert_eq!(reply.unwrap(), json!("ok"));
        assert!(out.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn request_after_close_fails_promptly_without_sending() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = Out::new(tx);
        out.close();

        let error = timeout(DEADLINE, out.request("test", Value::Null))
            .await
            .expect("request after close must not hang")
            .unwrap_err();
        assert_eq!(error.message, "ACP connection closed");
        assert!(out.pending.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn close_drains_requests_registered_before_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = Arc::new(Out::new(tx));
        let requester = out.clone();
        let request = tokio::spawn(async move { requester.request("test", Value::Null).await });
        timeout(DEADLINE, rx.recv())
            .await
            .expect("request must be sent")
            .unwrap();
        assert_eq!(out.pending.lock().unwrap().len(), 1);

        out.close();
        let error = timeout(DEADLINE, request)
            .await
            .expect("close must release the waiter")
            .unwrap()
            .unwrap_err();
        assert_eq!(error.message, "ACP connection closed");
        assert!(out.pending.lock().unwrap().is_empty());
    }
}
