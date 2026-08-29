use crate::ClipboardManager;
use anyhow::Result;
use futures::future::LocalBoxFuture;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Mutex;
use tower::{Layer, Service};

#[derive(Debug, Clone)]
pub enum Request {
    Poll,
    Copy(String),
    Clear,
    Prune(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum Response {
    Polled,
    Copied,
    Cleared,
    Pruned,
}

pub type Error = anyhow::Error;

pub type SharedManager = Arc<Mutex<ClipboardManager>>;

#[derive(Clone)]
pub struct ClipboardRouter {
    manager: SharedManager,
}

impl ClipboardRouter {
    pub fn new(manager: SharedManager) -> Self {
        Self { manager }
    }
}

impl Service<Request> for ClipboardRouter {
    type Response = Response;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Response, Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let manager = self.manager.clone();
        Box::pin(async move {
            let mut mgr = manager.lock().await;
            match req {
                Request::Poll => mgr.poll_clipboard().await.map(|_| Response::Polled),
                Request::Copy(sel) => mgr.copy_selection(&sel).await.map(|_| Response::Copied),
                Request::Clear => mgr.clear().await.map(|_| Response::Cleared),
                Request::Prune(hashes) => mgr.prune(&hashes).map(|_| Response::Pruned),
            }
        })
    }
}

pub struct ClipboardRecoveryLayer {
    manager: SharedManager,
}

impl ClipboardRecoveryLayer {
    pub fn new(manager: SharedManager) -> Self {
        Self { manager }
    }
}

impl<S> Layer<S> for ClipboardRecoveryLayer {
    type Service = ClipboardRecovery<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ClipboardRecovery {
            inner,
            manager: self.manager.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ClipboardRecovery<S> {
    inner: S,
    manager: SharedManager,
}

fn is_clipboard_dead(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("stopped") || s.contains("handler") || s.contains("not available")
}

impl<S> Service<Request> for ClipboardRecovery<S>
where
    S: Service<Request, Response = Response, Error = Error> + Clone + 'static,
    S::Future: 'static,
{
    type Response = Response;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Response, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let mut inner = self.inner.clone();
        let manager = self.manager.clone();
        Box::pin(async move {
            match inner.call(req.clone()).await {
                Ok(r) => Ok(r),
                Err(e) if is_clipboard_dead(&e) => {
                    eprintln!("Clipboard dead ({}), reconnecting and retrying", e);
                    manager.lock().await.try_reconnect().await;
                    inner.call(req).await
                }
                Err(e) => Err(e),
            }
        })
    }
}
