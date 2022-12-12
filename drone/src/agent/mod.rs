use self::executor::Executor;
use crate::{
    agent::engines::docker::DockerInterface, config::DockerConfig, database::DroneDatabase,
    ip::IpSource,
};
use anyhow::{anyhow, Result};
use http::Uri;
use hyper::Client;
use plane_core::{
    logging::LogError,
    messages::{
        agent::{DroneConnectRequest, DroneState, SpawnRequest, TerminationRequest},
        scheduler::DrainDrone,
        state::StateUpdate,
        PLANE_VERSION,
    },
    nats::TypedNats,
    retry::do_with_retry,
    types::{ClusterName, DroneId},
    NeverResult,
};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::sync::watch::{self, Receiver, Sender};

mod backend;
mod engine;
mod engines;
mod executor;

pub struct AgentOptions {
    pub drone_id: DroneId,
    pub db: DroneDatabase,
    pub nats: TypedNats,
    pub cluster_domain: ClusterName,

    /// Public IP of the machine the drone is running on.
    pub ip: IpSource,

    pub docker_options: DockerConfig,
}

pub async fn wait_port_ready(addr: &SocketAddr) -> Result<()> {
    tracing::info!(%addr, "Waiting for ready port.");

    let client = Client::new();
    let uri = Uri::from_maybe_shared(format!("http://{}:{}/", addr.ip(), addr.port()))?;

    do_with_retry(|| client.get(uri.clone()), 3000, Duration::from_millis(10)).await?;

    Ok(())
}

async fn listen_for_spawn_requests(
    drone_id: &DroneId,
    executor: Executor<DockerInterface>,
    nats: TypedNats,
) -> NeverResult {
    let mut sub = nats
        .subscribe(SpawnRequest::subscribe_subject(drone_id))
        .await?;
    executor.resume_backends().await?;
    tracing::info!("Listening for spawn requests.");

    loop {
        let req = sub.next().await;

        match req {
            Some(req) => {
                let executor = executor.clone();

                req.respond(&true).await?;
                tokio::spawn(async move {
                    executor.start_backend(&req.value).await;
                });
            }
            None => return Err(anyhow!("Spawn request subscription closed.")),
        }
    }
}

async fn listen_for_termination_requests(
    executor: Executor<DockerInterface>,
    nats: TypedNats,
    cluster: ClusterName,
) -> NeverResult {
    let mut sub = nats
        .subscribe(TerminationRequest::subscribe_subject(&cluster))
        .await?;
    tracing::info!("Listening for termination requests.");
    loop {
        let req = sub.next().await;
        match req {
            Some(req) => {
                let executor = executor.clone();

                req.respond(&()).await?;
                tokio::spawn(async move { executor.kill_backend(&req.value).await });
            }
            None => return Err(anyhow!("Termination request subscription closed.")),
        }
    }
}

/// Repeatedly publish a heartbeat message about this drone.
async fn heartbeat_loop(
    nc: TypedNats,
    drone_id: &DroneId,
    cluster: ClusterName,
    recv_state: Receiver<DroneState>,
    ip: IpAddr,
) -> NeverResult {
    let mut interval = tokio::time::interval(Duration::from_secs(4));

    loop {
        let state = *recv_state.borrow();
        tracing::info!(state=?state, "Publishing heartbeat.");

        nc.publish_jetstream(&StateUpdate::DroneStatus {
            cluster: cluster.clone(),
            drone: drone_id.clone(),
            state,
            ip,
            drone_version: PLANE_VERSION.to_string(),
        })
        .await
        .log_error("Error publishing StateUpdate::DroneStatus.");

        interval.tick().await;
    }
}

/// Listen for drain instruction.
async fn listen_for_drain(
    nc: TypedNats,
    drone_id: DroneId,
    cluster: ClusterName,
    send_state: Sender<DroneState>,
) -> NeverResult {
    let mut sub = nc
        .subscribe(DrainDrone::subscribe_subject(drone_id, cluster))
        .await?;

    while let Some(req) = sub.next().await {
        tracing::info!(req=?req.value, "Received request to drain drone.");
        req.respond(&()).await?;

        let state = if req.value.drain {
            DroneState::Draining
        } else {
            DroneState::Ready
        };

        send_state
            .send(state)
            .log_error("Error sending drain instruction.");
    }

    Err(anyhow!("Reached the end of DrainDrone subscription."))
}

pub async fn run_agent(agent_opts: AgentOptions) -> NeverResult {
    let nats = &agent_opts.nats;

    tracing::info!("Connecting to Docker.");
    let docker = DockerInterface::try_new(&agent_opts.docker_options).await?;
    tracing::info!("Connecting to sqlite.");
    let db = agent_opts.db;
    let cluster = agent_opts.cluster_domain.clone();
    let ip = do_with_retry(|| agent_opts.ip.get_ip(), 10, Duration::from_secs(10)).await?;

    let request = DroneConnectRequest {
        drone_id: agent_opts.drone_id.clone(),
        cluster: cluster.clone(),
        ip,
    };

    nats.publish(&request).await?;

    let executor = Executor::new(docker, db.clone(), nats.clone(), ip, cluster.clone());

    let (send_state, recv_state) = watch::channel(DroneState::Ready);

    tokio::select!(
        result = heartbeat_loop(
            nats.clone(),
            &agent_opts.drone_id,
            cluster.clone(),
            recv_state.clone(),
            ip,
        ) => result,

        result = listen_for_spawn_requests(
            &agent_opts.drone_id,
            executor.clone(),
            nats.clone()
        ) => result,

        result = listen_for_termination_requests(
            executor.clone(),
            nats.clone(),
            cluster.clone(),
        ) => result,

        result = listen_for_drain(
            nats.clone(),
            agent_opts.drone_id.clone(),
            cluster.clone(),
            send_state,
        ) => result,
    )
}
