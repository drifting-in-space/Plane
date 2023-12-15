use self::{
    backend_status::{handle_backend_status, handle_backend_status_stream},
    dns::handle_dns_socket,
    drain::handle_drain,
    proxy::handle_proxy_socket,
};
use crate::{
    client::PlaneClient,
    controller::{connect::handle_connect, core::Controller, drone::handle_drone_socket},
    database::PlaneDatabase,
    heartbeat_consts::HEARTBEAT_INTERVAL,
    names::ControllerName,
    signals::wait_for_shutdown_signal,
    PLANE_GIT_HASH, PLANE_VERSION,
};
use anyhow::Result;
use axum::{
    routing::{get, post},
    Json, Router, Server,
};
use serde_json::{json, Value};
use std::net::{SocketAddr, TcpListener};
use tokio::{
    sync::oneshot::{self},
    task::JoinHandle,
};
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;
use url::Url;

mod backend_status;
mod connect;
mod core;
mod dns;
mod drain;
mod drone;
pub mod error;
mod proxy;
mod terminate;

pub async fn status() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": PLANE_VERSION,
        "hash": PLANE_GIT_HASH,
    }))
}

struct HeartbeatSender {
    handle: JoinHandle<Result<()>>,
    db: PlaneDatabase,
    controller_id: ControllerName,
}

impl HeartbeatSender {
    pub async fn start(db: PlaneDatabase, controller_id: ControllerName) -> Result<Self> {
        // Wait until we have sent the initial heartbeat.
        db.controller().heartbeat(&controller_id, true).await?;

        let db_clone = db.clone();
        let controller_id_clone = controller_id.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                db_clone
                    .controller()
                    .heartbeat(&controller_id_clone, true)
                    .await?;
            }
        });

        Ok(Self {
            handle,
            db,
            controller_id,
        })
    }

    pub async fn terminate(&self) {
        self.handle.abort();
        if let Err(err) = self
            .db
            .controller()
            .heartbeat(&self.controller_id, false)
            .await
        {
            tracing::error!(?err, "Failed to send offline heartbeat");
        }
    }
}

pub struct ControllerServer {
    bind_addr: SocketAddr,
    controller_id: ControllerName,
    graceful_terminate_sender: Option<oneshot::Sender<()>>,
    heartbeat_handle: HeartbeatSender,
    // server_handle is wrapped in an Option<> because we need to take ownership of it to join it
    // when gracefully terminating.
    server_handle: Option<JoinHandle<hyper::Result<()>>>,
}

impl ControllerServer {
    pub async fn run(db: PlaneDatabase, bind_addr: SocketAddr, id: ControllerName) -> Result<Self> {
        let listener = TcpListener::bind(bind_addr)?;

        Self::run_with_listener(db, listener, id).await
    }

    pub async fn run_with_listener(
        db: PlaneDatabase,
        listener: TcpListener,
        id: ControllerName,
    ) -> Result<Self> {
        let bind_addr = listener.local_addr()?;

        let (graceful_terminate_sender, graceful_terminate_receiver) =
            tokio::sync::oneshot::channel::<()>();

        let controller = Controller::new(db.clone(), id.clone()).await;

        let trace_layer = TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_request(DefaultOnRequest::new().level(Level::INFO))
            .on_response(DefaultOnResponse::new().level(Level::INFO));

        let heartbeat_handle = HeartbeatSender::start(db.clone(), id.clone()).await?;

        let app = Router::new()
            .route("/status", get(status))
            .route("/c/:cluster/drone-socket", get(handle_drone_socket))
            .route("/c/:cluster/proxy-socket", get(handle_proxy_socket))
            .route("/dns-socket", get(handle_dns_socket))
            .route("/c/:cluster/connect", post(handle_connect))
            .route("/c/:cluster/b/:backend/status", get(handle_backend_status))
            .route(
                "/c/:cluster/b/:backend/status-stream",
                get(handle_backend_status_stream),
            )
            .route("/c/:cluster/d/:drone/drain", post(handle_drain))
            .route(
                "/c/:cluster/b/:backend/soft-terminate",
                post(terminate::handle_soft_terminate),
            )
            .route(
                "/c/:cluster/b/:backend/hard-terminate",
                post(terminate::handle_hard_terminate),
            )
            .layer(trace_layer)
            .with_state(controller);

        let server_handle = tokio::spawn(
            Server::from_tcp(listener)?
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async {
                    graceful_terminate_receiver.await.ok();
                }),
        );

        Ok(Self {
            graceful_terminate_sender: Some(graceful_terminate_sender),
            heartbeat_handle,
            server_handle: Some(server_handle),
            controller_id: id,
            bind_addr,
        })
    }

    pub async fn terminate(&mut self) {
        // Stop sending online heartbeat.
        self.heartbeat_handle.terminate().await;

        // Begin graceful shutdown of server.
        let Some(graceful_terminate_sender) = self.graceful_terminate_sender.take() else {
            return;
        };

        if let Err(err) = graceful_terminate_sender.send(()) {
            tracing::error!(?err, "Failed to send graceful terminate signal");
        } else {
            // Wait for server to finish shutting down.
            let Some(server_handle) = self.server_handle.take() else {
                return;
            };

            match server_handle.await {
                Ok(Ok(())) => {
                    tracing::info!("Server gracefully terminated");
                }
                Ok(Err(err)) => {
                    tracing::error!(?err, "Server error");
                }
                Err(err) => {
                    tracing::error!(?err, "Server error");
                }
            }
        }
    }

    pub fn id(&self) -> &ControllerName {
        &self.controller_id
    }

    pub fn client(&self) -> PlaneClient {
        let base_url: Url = format!("http://{}", self.bind_addr)
            .parse()
            .expect("Generated URI is always valid.");
        PlaneClient::new(base_url)
    }
}

pub async fn run_controller(
    db: PlaneDatabase,
    bind_addr: SocketAddr,
    id: ControllerName,
) -> Result<()> {
    let mut server = ControllerServer::run(db, bind_addr, id).await?;

    wait_for_shutdown_signal().await;

    server.terminate().await;

    Ok(())
}