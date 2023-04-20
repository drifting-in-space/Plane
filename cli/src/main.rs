use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use plane_core::{
    messages::{
        agent::{BackendStateMessage, DockerExecutableConfig, ResourceLimits, TerminationRequest},
        scheduler::{DrainDrone, ScheduleRequest, ScheduleResponse},
        state::{
            BackendMessage, BackendMessageType, ClusterStateMessage, DroneMessage,
            DroneMessageType, WorldStateMessage,
        },
    },
    nats_connection::NatsConnectionSpec,
    state::get_world_state,
    types::{BackendId, ClusterName, DroneId},
};
use std::{collections::HashMap, time::Duration};

#[derive(Parser)]
struct Opts {
    #[clap(long)]
    nats: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    ListDrones,
    ListBackends,
    Spawn {
        cluster: String,
        image: String,
        /// Grace period with no connections before shutting down the drone.
        #[clap(long, default_value = "300")]
        timeout: u64,
        #[clap(long, short)]
        port: Option<u16>,
        #[clap(long, short)]
        env: Vec<String>,
    },
    Status {
        backend: Option<String>,
    },
    Drain {
        cluster: String,
        drone: String,

        /// Cancel draining and allow a drone to accept backends again.
        #[clap(long)]
        cancel: bool,
    },
    Terminate {
        cluster: String,
        backend: String,
    },
    DumpState,
    StreamState {
        /// Whether to include heartbeat messages.
        #[clap(long, default_value = "false")]
        include_heartbeat: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    tracing_subscriber::fmt().init();

    let nats = NatsConnectionSpec::from_url(opts.nats.as_deref().unwrap_or("nats://localhost"))?
        .connect("cli.inbox")
        .await?;

    match opts.command {
        Command::DumpState => {
            let state = get_world_state(nats.clone()).await?;
            println!("{:#?}", state);
        }
        Command::StreamState { include_heartbeat } => {
            let mut sub = nats
                .subscribe_jetstream_subject(WorldStateMessage::subscribe_subject())
                .await?;

            while let Some((message, _)) = sub.next().await {
                let WorldStateMessage { cluster, message } = message;

                let text = match message {
                    ClusterStateMessage::AcmeMessage(acme) => {
                        format!("ACME TXT entry: {}", acme.value.to_string().bright_blue())
                    }
                    ClusterStateMessage::DroneMessage(drone) => {
                        let DroneMessage { drone, message } = drone;

                        let text = match message {
                            DroneMessageType::KeepAlive { timestamp } => {
                                if !include_heartbeat {
                                    continue;
                                }
                                format!("is alive at {}", timestamp.to_string().bright_blue())
                            }
                            DroneMessageType::Metadata(metadata) => {
                                format!(
                                    "has IP: {}, version: {}, git hash: {}",
                                    metadata.ip.to_string().bright_blue(),
                                    metadata.version.to_string().bright_blue(),
                                    metadata
                                        .git_hash_short()
                                        .unwrap_or("unknown".to_string())
                                        .bright_blue()
                                )
                            }
                            DroneMessageType::State { state, timestamp } => {
                                format!(
                                    "is {} at {}",
                                    state.to_string().bright_magenta(),
                                    timestamp.to_string().bright_yellow()
                                )
                            }
                        };

                        format!("Drone: {} {}", drone.to_string().bright_yellow(), text)
                    }
                    ClusterStateMessage::BackendMessage(backend) => {
                        let BackendMessage { backend, message } = backend;

                        let text = match message {
                            BackendMessageType::State { state, timestamp } => {
                                format!(
                                    "is {} at {}",
                                    state.to_string().bright_magenta(),
                                    timestamp.to_string().bright_yellow()
                                )
                            }
                            BackendMessageType::Assignment { drone } => {
                                format!(
                                    "is assigned to drone {}",
                                    drone.to_string().bright_yellow()
                                )
                            }
                        };

                        format!("Backend: {} {}", backend.to_string().bright_cyan(), text)
                    }
                };

                println!("Cluster: {} {}", cluster.to_string().bright_green(), text);
            }
        }
        Command::Status { backend } => {
            let mut sub = if let Some(backend) = backend {
                nats.subscribe_jetstream_subject(BackendStateMessage::subscribe_subject(
                    &BackendId::new(backend),
                ))
                .await?
            } else {
                nats.subscribe_jetstream().await?
            };

            while let Some((message, _)) = sub.next().await {
                println!(
                    "{}\t{}\t{}",
                    message.backend.to_string().bright_cyan(),
                    message.state.to_string().bright_magenta(),
                    message.time.to_string().blue()
                );
            }
        }
        Command::ListDrones => {
            let state = get_world_state(nats.clone()).await?;
            for (cluster_name, cluster) in &state.clusters {
                println!("{}", cluster_name.to_string().bright_green());
                for (drone_id, drone) in &cluster.drones {
                    println!(
                        "\t{}\t{}\t{}\t{}\t{}\t{}",
                        drone_id.to_string().bright_blue(),
                        drone
                            .state()
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                            .bright_magenta(),
                        drone
                            .last_seen
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                            .blue(),
                        drone
                            .meta
                            .as_ref()
                            .map(|d| d.ip.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                            .bright_yellow(),
                        drone
                            .meta
                            .as_ref()
                            .map(|d| d.version.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                            .bright_cyan(),
                        drone
                            .meta
                            .as_ref()
                            .and_then(|d| d.git_hash_short())
                            .unwrap_or_else(|| "unknown".to_string())
                            .bright_purple(),
                    );
                }
            }
        }
        Command::Spawn {
            image,
            cluster,
            timeout,
            port,
            env,
        } => {
            let env: Result<HashMap<String, String>> = env
                .iter()
                .map(|d| {
                    let (key, value) = d
                        .split_once('=')
                        .context("Expected environment variables in the form KEY=VALUE")?;

                    Ok((key.to_string(), value.to_string()))
                })
                .collect();
            let env = env?;

            let result = nats
                .request(&ScheduleRequest {
                    backend_id: None,
                    cluster: ClusterName::new(&cluster),
                    max_idle_secs: Duration::from_secs(timeout),
                    metadata: HashMap::new(),
                    executable: DockerExecutableConfig {
                        image,
                        env,
                        credentials: None,
                        resource_limits: ResourceLimits::default(),
                        pull_policy: Default::default(),
                        port,
                        volume_mounts: Vec::new(),
                    },
                    require_bearer_token: false,
                })
                .await?;

            match result {
                ScheduleResponse::Scheduled {
                    drone,
                    backend_id,
                    bearer_token,
                } => {
                    let url = format!("https://{}.{}", backend_id, cluster);

                    println!("Backend scheduled.");
                    println!("URL: {}", url.bright_green());
                    println!("Drone: {}", drone.to_string().bright_blue());
                    println!("Backend ID: {}", backend_id.to_string().bright_blue());
                    if let Some(bearer_token) = bearer_token {
                        println!("Bearer token: {}", bearer_token.bright_blue());
                    }
                }
                ScheduleResponse::NoDroneAvailable => tracing::error!(
                    %cluster,
                    "Could not schedule backend because no drone was available for cluster."
                ),
            }
        }
        Command::ListBackends => {
            let state = get_world_state(nats.clone()).await?;
            for (cluster_name, cluster) in &state.clusters {
                println!("{}", cluster_name.to_string().bright_green());
                for (backend_id, backend) in &cluster.backends {
                    println!(
                        "\t{}.{}\t{}",
                        backend_id.to_string().bright_blue(),
                        cluster_name.to_string().bright_green(),
                        backend
                            .state()
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                            .bright_magenta(),
                    );
                }
            }
        }
        Command::Terminate {
            cluster, backend, ..
        } => {
            nats.request(&TerminationRequest {
                backend_id: BackendId::new(backend),
                cluster_id: ClusterName::new(&cluster),
            })
            .await?;

            println!("{}", "Terminated successfully".bright_green());
        }
        Command::Drain {
            drone,
            cluster,
            cancel,
        } => {
            let drain = !cancel;
            nats.request(&DrainDrone {
                cluster: ClusterName::new(&cluster),
                drone: DroneId::new(drone),
                drain,
            })
            .await?;

            if drain {
                println!("{}", "Draining started on drone.".bright_green());
            } else {
                println!("{}", "Draining cancelled on drone.".bright_green());
            }
        }
    }

    Ok(())
}
