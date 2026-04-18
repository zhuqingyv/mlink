use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex, RwLock};

use crate::core::node::Node;
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::types::MessageType;

pub const RPC_STATUS_OK: u8 = 0;
pub const RPC_STATUS_ERROR: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub request_id: u16,
    pub method: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub request_id: u16,
    pub status: u8,
    pub data: Vec<u8>,
}

pub type BoxedHandler =
    Box<dyn Fn(Vec<u8>) -> BoxFuture<'static, Result<Vec<u8>>> + Send + Sync + 'static>;

pub struct RpcRegistry {
    handlers: RwLock<HashMap<String, Arc<BoxedHandler>>>,
}

impl RpcRegistry {
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register(&self, method: String, handler: BoxedHandler) {
        let mut guard = self.handlers.write().await;
        guard.insert(method, Arc::new(handler));
    }

    pub async fn unregister(&self, method: &str) -> bool {
        let mut guard = self.handlers.write().await;
        guard.remove(method).is_some()
    }

    pub async fn has(&self, method: &str) -> bool {
        let guard = self.handlers.read().await;
        guard.contains_key(method)
    }

    pub async fn handle_request(&self, method: &str, data: Vec<u8>) -> Result<Vec<u8>> {
        let handler = {
            let guard = self.handlers.read().await;
            guard
                .get(method)
                .cloned()
                .ok_or_else(|| MlinkError::UnknownMethod(method.to_string()))?
        };
        handler(data).await
    }
}

impl Default for RpcRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct PendingRequests {
    next_id: Mutex<u16>,
    waiters: Mutex<HashMap<u16, oneshot::Sender<RpcResponse>>>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn next_id(&self) -> u16 {
        let mut guard = self.next_id.lock().await;
        let id = *guard;
        *guard = guard.wrapping_add(1);
        id
    }

    pub async fn register(&self, id: u16) -> oneshot::Receiver<RpcResponse> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.waiters.lock().await;
        guard.insert(id, tx);
        rx
    }

    pub async fn complete(&self, response: RpcResponse) -> bool {
        let tx = {
            let mut guard = self.waiters.lock().await;
            guard.remove(&response.request_id)
        };
        match tx {
            Some(tx) => tx.send(response).is_ok(),
            None => false,
        }
    }

    pub async fn cancel(&self, id: u16) -> bool {
        let mut guard = self.waiters.lock().await;
        guard.remove(&id).is_some()
    }

    pub async fn pending_count(&self) -> usize {
        let guard = self.waiters.lock().await;
        guard.len()
    }
}

pub async fn send_request(
    node: &Node,
    peer_id: &str,
    request_id: u16,
    method: &str,
    data: &[u8],
) -> Result<()> {
    let req = RpcRequest {
        request_id,
        method: method.to_string(),
        data: data.to_vec(),
    };
    let payload = rmp_serde::to_vec(&req)
        .map_err(|e| MlinkError::CodecError(format!("encode RpcRequest: {e}")))?;
    node.send_raw(peer_id, MessageType::Request, &payload).await
}

pub async fn send_response(
    node: &Node,
    peer_id: &str,
    response: &RpcResponse,
) -> Result<()> {
    let payload = rmp_serde::to_vec(response)
        .map_err(|e| MlinkError::CodecError(format!("encode RpcResponse: {e}")))?;
    node.send_raw(peer_id, MessageType::Response, &payload).await
}

pub fn decode_request(bytes: &[u8]) -> Result<RpcRequest> {
    rmp_serde::from_slice(bytes)
        .map_err(|e| MlinkError::CodecError(format!("decode RpcRequest: {e}")))
}

pub fn decode_response(bytes: &[u8]) -> Result<RpcResponse> {
    rmp_serde::from_slice(bytes)
        .map_err(|e| MlinkError::CodecError(format!("decode RpcResponse: {e}")))
}

pub async fn rpc_request(
    node: &Node,
    pending: &PendingRequests,
    peer_id: &str,
    method: &str,
    data: &[u8],
    timeout_ms: u64,
) -> Result<Vec<u8>> {
    let id = pending.next_id().await;
    let rx = pending.register(id).await;
    send_request(node, peer_id, id, method, data).await?;

    let resp = match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => {
            return Err(MlinkError::HandlerError(
                "response channel closed".to_string(),
            ));
        }
        Err(_) => {
            pending.cancel(id).await;
            return Err(MlinkError::Timeout);
        }
    };

    if resp.status == RPC_STATUS_OK {
        Ok(resp.data)
    } else {
        let msg = String::from_utf8(resp.data).unwrap_or_else(|_| "rpc error".to_string());
        Err(MlinkError::HandlerError(msg))
    }
}

pub async fn dispatch_request(
    node: &Node,
    registry: &RpcRegistry,
    peer_id: &str,
    request_bytes: &[u8],
) -> Result<()> {
    let req = decode_request(request_bytes)?;
    let (status, data) = match registry.handle_request(&req.method, req.data).await {
        Ok(out) => (RPC_STATUS_OK, out),
        Err(MlinkError::UnknownMethod(m)) => (RPC_STATUS_ERROR, m.into_bytes()),
        Err(e) => (RPC_STATUS_ERROR, e.to_string().into_bytes()),
    };
    let resp = RpcResponse {
        request_id: req.request_id,
        status,
        data,
    };
    send_response(node, peer_id, &resp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_handler() -> BoxedHandler {
        Box::new(|data: Vec<u8>| Box::pin(async move { Ok(data) }))
    }

    fn err_handler() -> BoxedHandler {
        Box::new(|_data: Vec<u8>| {
            Box::pin(async move {
                Err::<Vec<u8>, _>(MlinkError::HandlerError("boom".into()))
            })
        })
    }

    #[tokio::test]
    async fn registry_register_and_handle() {
        let reg = RpcRegistry::new();
        reg.register("echo".into(), noop_handler()).await;
        let out = reg.handle_request("echo", b"hi".to_vec()).await.unwrap();
        assert_eq!(out, b"hi");
    }

    #[tokio::test]
    async fn registry_unknown_method() {
        let reg = RpcRegistry::new();
        let err = reg.handle_request("nope", vec![]).await.unwrap_err();
        assert!(matches!(err, MlinkError::UnknownMethod(ref m) if m == "nope"));
    }

    #[tokio::test]
    async fn registry_handler_error_propagates() {
        let reg = RpcRegistry::new();
        reg.register("bad".into(), err_handler()).await;
        let err = reg.handle_request("bad", vec![]).await.unwrap_err();
        assert!(matches!(err, MlinkError::HandlerError(_)));
    }

    #[tokio::test]
    async fn registry_unregister_removes() {
        let reg = RpcRegistry::new();
        reg.register("x".into(), noop_handler()).await;
        assert!(reg.has("x").await);
        assert!(reg.unregister("x").await);
        assert!(!reg.has("x").await);
        assert!(!reg.unregister("x").await);
    }

    #[tokio::test]
    async fn pending_requests_next_id_is_monotonic_with_wrap() {
        let p = PendingRequests::new();
        assert_eq!(p.next_id().await, 0);
        assert_eq!(p.next_id().await, 1);
        assert_eq!(p.next_id().await, 2);
    }

    #[tokio::test]
    async fn pending_requests_complete_delivers_response() {
        let p = PendingRequests::new();
        let rx = p.register(42).await;
        let resp = RpcResponse {
            request_id: 42,
            status: RPC_STATUS_OK,
            data: b"out".to_vec(),
        };
        assert!(p.complete(resp.clone()).await);
        let got = rx.await.expect("recv");
        assert_eq!(got, resp);
        assert_eq!(p.pending_count().await, 0);
    }

    #[tokio::test]
    async fn pending_requests_complete_missing_returns_false() {
        let p = PendingRequests::new();
        let resp = RpcResponse {
            request_id: 99,
            status: RPC_STATUS_OK,
            data: vec![],
        };
        assert!(!p.complete(resp).await);
    }

    #[tokio::test]
    async fn pending_requests_cancel_removes() {
        let p = PendingRequests::new();
        let _rx = p.register(7).await;
        assert_eq!(p.pending_count().await, 1);
        assert!(p.cancel(7).await);
        assert_eq!(p.pending_count().await, 0);
        assert!(!p.cancel(7).await);
    }

    #[test]
    fn request_response_round_trip_via_msgpack() {
        let req = RpcRequest {
            request_id: 3,
            method: "m".into(),
            data: b"abc".to_vec(),
        };
        let bytes = rmp_serde::to_vec(&req).unwrap();
        let back = decode_request(&bytes).unwrap();
        assert_eq!(req, back);

        let resp = RpcResponse {
            request_id: 3,
            status: RPC_STATUS_OK,
            data: b"xyz".to_vec(),
        };
        let bytes = rmp_serde::to_vec(&resp).unwrap();
        let back = decode_response(&bytes).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn decode_request_rejects_garbage() {
        let err = decode_request(&[0xFF, 0xFF, 0xFF]).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }
}
