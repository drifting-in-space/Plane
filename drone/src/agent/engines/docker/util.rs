use std::collections::HashMap;

use bollard::service::EventMessage;

pub trait MinuteExt {
    fn as_minutes(&self) -> u128;
}

impl MinuteExt for std::time::Duration {
    fn as_minutes(&self) -> u128 {
        (self.as_secs() / 60).into()
    }
}

pub fn make_exposed_ports(port: u16) -> Option<HashMap<String, HashMap<(), ()>>> {
    let dummy: HashMap<(), ()> = vec![].into_iter().collect();
    Some(vec![(format!("{}/tcp", port), dummy)].into_iter().collect())
}

/// Helper trait for swallowing Docker not found errors.
pub trait AllowNotFound {
    /// Swallow a result if it is a success result or a NotFound; propagate it otherwise.
    fn allow_not_found(self) -> Result<(), bollard::errors::Error>;
}

impl<T> AllowNotFound for Result<T, bollard::errors::Error> {
    fn allow_not_found(self) -> Result<(), bollard::errors::Error> {
        match self {
            Ok(_) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                message,
            }) => {
                tracing::warn!(
                    ?message,
                    "Received 404 error from docker, possibly expected."
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// The list of possible container events.
/// Comes from [Docker documentation](https://docs.docker.com/engine/reference/commandline/events/).
#[derive(Debug, PartialEq, Eq)]
pub enum ContainerEventType {
    Attach,
    Commit,
    Copy,
    Create,
    Destroy,
    Detach,
    Die,
    ExecCreate,
    ExecDetach,
    ExecDie,
    ExecStart,
    Export,
    HealthStatus,
    Kill,
    Oom,
    Pause,
    Rename,
    Resize,
    Restart,
    Start,
    Stop,
    Top,
    Unpause,
    Update,
}

#[allow(unused)]
#[derive(Debug)]
pub struct ContainerEvent {
    pub event: ContainerEventType,
    pub name: String,
}

impl ContainerEvent {
    pub fn from_event_message(event: &EventMessage) -> Option<Self> {
        let action = event.action.as_deref()?;
        let actor = event.actor.as_ref()?;
        let name: String = actor.attributes.as_ref()?.get("name")?.to_string();

        let event = match action {
            "attach" => ContainerEventType::Attach,
            "commit" => ContainerEventType::Commit,
            "copy" => ContainerEventType::Copy,
            "create" => ContainerEventType::Create,
            "destroy" => ContainerEventType::Destroy,
            "detach" => ContainerEventType::Detach,
            "die" => ContainerEventType::Die,
            "exec_create" => ContainerEventType::ExecCreate,
            "exec_detach" => ContainerEventType::ExecDetach,
            "exec_die" => ContainerEventType::ExecDie,
            "exec_start" => ContainerEventType::ExecStart,
            "export" => ContainerEventType::Export,
            "health_status" => ContainerEventType::HealthStatus,
            "kill" => ContainerEventType::Kill,
            "oom" => ContainerEventType::Oom,
            "pause" => ContainerEventType::Pause,
            "rename" => ContainerEventType::Rename,
            "resize" => ContainerEventType::Resize,
            "restart" => ContainerEventType::Restart,
            "start" => ContainerEventType::Start,
            "stop" => ContainerEventType::Stop,
            "top" => ContainerEventType::Top,
            "unpause" => ContainerEventType::Unpause,
            "update" => ContainerEventType::Update,
            _ => {
                tracing::info!(?action, "Unhandled container action.");
                return None;
            }
        };

        Some(ContainerEvent { event, name })
    }
}
