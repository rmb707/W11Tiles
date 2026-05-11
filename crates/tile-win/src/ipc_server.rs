//! Named-pipe IPC server.
//!
//! Listens on `\\.\pipe\tilemanager.sock` (per `tile_core::ipc::PIPE_NAME`).
//! Each connection: read newline-delimited JSON `Request`, hand off to the
//! daemon via channel, send back the `Response`.

#![cfg(windows)]

use std::sync::Arc;

use tile_core::ipc::{Request, Response, PIPE_NAME};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

pub type IpcRequest = (Request, oneshot::Sender<Response>);

pub async fn serve(req_tx: mpsc::UnboundedSender<IpcRequest>) {
    let req_tx = Arc::new(req_tx);
    info!(pipe = PIPE_NAME, "IPC listening");

    loop {
        let server = match ServerOptions::new()
            .first_pipe_instance(false)
            // tokio caps this at 254; PIPE_UNLIMITED_INSTANCES (255) panics.
            .max_instances(254)
            .create(PIPE_NAME)
        {
            Ok(s) => s,
            Err(e) => { error!("create pipe failed: {e}"); return; }
        };
        if let Err(e) = server.connect().await {
            error!("pipe connect failed: {e}");
            continue;
        }
        let req_tx = req_tx.clone();
        tokio::spawn(async move { handle_client(server, req_tx).await; });
    }
}

async fn handle_client(server: NamedPipeServer, req_tx: Arc<mpsc::UnboundedSender<IpcRequest>>) {
    let (read, mut write) = tokio::io::split(server);
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    while let Ok(n) = reader.read_line(&mut line).await {
        if n == 0 { break; }
        let resp = match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => {
                let (otx, orx) = oneshot::channel();
                if req_tx.send((req, otx)).is_err() {
                    Response::Error { message: "daemon shut down".into() }
                } else {
                    orx.await.unwrap_or(Response::Error { message: "daemon dropped reply".into() })
                }
            }
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        if let Ok(s) = serde_json::to_string(&resp) {
            if write.write_all(s.as_bytes()).await.is_err() { break; }
            if write.write_all(b"\n").await.is_err() { break; }
        }
        line.clear();
    }
}
