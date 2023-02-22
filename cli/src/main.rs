use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use plane_core::{
    messages::{
        agent::{BackendStateMessage, DockerExecutableConfig, ResourceLimits, TerminationRequest},
        scheduler::{DrainDrone, ScheduleRequest, ScheduleResponse},
    },
    nats_connection::NatsConnectionSpec,
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
    ListDns,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    tracing_subscriber::fmt().init();

    let nats = NatsConnectionSpec::from_url(opts.nats.as_deref().unwrap_or("nats://localhost"))?
        .connect("cli.inbox")
        .await?;

    match opts.command {
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
            todo!("Implement list drones");
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
                    let (key, value) = d.split_once('=').ok_or_else(|| {
                        anyhow!(
                            "Expected environment variables in the form KEY=VALUE, instead got {}",
                            d
                        )
                    })?;

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
        Command::ListDns => {
            todo!("Implement list dns");
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
