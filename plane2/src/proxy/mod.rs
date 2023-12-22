use self::proxy_connection::ProxyConnection;
use crate::names::ProxyName;
use crate::proxy::cert_manager::watcher_manager_pair;
use crate::proxy::proxy_service::ProxyMakeService;
use crate::proxy::shutdown_signal::ShutdownSignal;
use crate::{client::PlaneClient, signals::wait_for_shutdown_signal, types::ClusterName};
use anyhow::Result;
use std::net::IpAddr;
use std::path::Path;
use url::Url;

pub mod cert_manager;
mod cert_pair;
mod connection_monitor;
pub mod proxy_connection;
mod proxy_service;
mod rewriter;
mod route_map;
mod shutdown_signal;
mod tls;

#[derive(Debug, Clone, Copy)]
pub enum Protocol {
    Http,
    Https,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Https => "https",
        }
    }
}

/// Information about the incoming request that is forwarded to the request in
/// X-Forwarded-* headers.
#[derive(Debug, Clone, Copy)]
pub struct ForwardableRequestInfo {
    /// The IP address of the client that made the request.
    /// Forwarded as X-Forwarded-For.
    ip: IpAddr,

    /// The protocol of the incoming request.
    /// Forwarded as X-Forwarded-Proto.
    protocol: Protocol,
}

#[derive(Debug, Copy, Clone)]
pub struct ServerPortConfig {
    pub http_port: u16,
    pub https_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct AcmeConfig {
    pub endpoint: Url,
    pub mailto_email: String,
    pub client: reqwest::Client,
    // TODO: EAB credentials.
}

pub async fn run_proxy(
    name: ProxyName,
    client: PlaneClient,
    cluster: ClusterName,
    cert_path: Option<&Path>,
    port_config: ServerPortConfig,
    acme_config: Option<AcmeConfig>,
) -> Result<()> {
    let (cert_watcher, cert_manager) =
        watcher_manager_pair(cluster.clone(), cert_path, acme_config)?;

    let proxy_connection = ProxyConnection::new(name, client, cluster, cert_manager);
    let shutdown_signal = ShutdownSignal::new();

    let https_redirect = port_config.https_port.is_some();

    let http_handle = ProxyMakeService {
        state: proxy_connection.state(),
        https_redirect,
    }
    .serve_http(port_config.http_port, shutdown_signal.subscribe())?;

    let https_handle = if let Some(https_port) = port_config.https_port {
        let https_handle = ProxyMakeService {
            state: proxy_connection.state(),
            https_redirect: false,
        }
        .serve_https(https_port, cert_watcher, shutdown_signal.subscribe())?;

        Some(https_handle)
    } else {
        None
    };

    wait_for_shutdown_signal().await;
    shutdown_signal.shutdown();
    tracing::info!("Shutting down proxy server.");

    http_handle.await?;
    if let Some(https_handle) = https_handle {
        https_handle.await?;
    }

    Ok(())
}
