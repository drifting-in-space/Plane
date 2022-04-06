use crate::{
    event_stream::{event_stream, past_events},
    SessionLivedBackend, SessionLivedBackendBuilder, SessionLivedBackendState, SPAWNER_GROUP,
};
use axum::{
    body::Body,
    extract::{Extension, Path},
    http::{header::HeaderName, HeaderValue, Response, StatusCode},
    response::{sse::Event as AxumSseEvent, Sse},
    routing::get,
    BoxError, Json, Router,
};
use futures::{Stream, TryStreamExt};
use k8s_openapi::api::core::v1::Event as KubeEventResource;
use kube::{
    api::PostParams, runtime::watcher::Error as KubeWatcherError, Api, Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tokio_stream::StreamExt;

pub fn backend_routes() -> Router {
    Router::new()
        .route("/:backend_id/status/stream", get(status_handler))
        .route("/:backend_id/status", get(last_status_handler))
        .route("/:backend_id/ready", get(ready_handler))
}

async fn ready_handler(
    Path((backend_id,)): Path<(String,)>,
    Extension(settings): Extension<Arc<ApiSettings>>,
) -> Result<Response<Body>, StatusCode> {
    let client = settings.get_client();
    let name = settings.backend_to_slab_name(&backend_id);

    let api = Api::<SessionLivedBackend>::namespaced(client, &settings.namespace);
    let slab = api.get(&name).await;

    match slab {
        Ok(slab) => {
            if slab.state() == SessionLivedBackendState::Ready {
                let url = settings
                    .backend_to_url(&backend_id)
                    .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

                return Response::builder()
                    .status(StatusCode::FOUND)
                    .header(
                        HeaderName::from_static("location"),
                        HeaderValue::from_str(&url)
                            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                    )
                    .body(Body::empty())
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            } else {
                return Err(StatusCode::CONFLICT);
            }
        }
        Err(error) => {
            tracing::warn!(?error, "Error when looking up SessionLivedBackend.");
            return Err(StatusCode::NOT_FOUND);
        }
    }
}

fn event_to_json(event: &KubeEventResource) -> Value {
    json!({
        "state": event.action,
        "time": event.event_time,
    })
}

fn convert_stream<T>(stream: T) -> impl Stream<Item = Result<AxumSseEvent, BoxError>>
where
    T: Stream<Item = Result<KubeEventResource, KubeWatcherError>>,
{
    stream.map(|event| {
        let event: KubeEventResource = event.map_err(Box::new)?;

        Ok(AxumSseEvent::default()
            .json_data(event_to_json(&event))
            .map_err(Box::new)?)
    })
}

async fn last_status_handler(
    Path((backend_id,)): Path<(String,)>,
    Extension(settings): Extension<Arc<ApiSettings>>,
) -> Result<Json<Value>, StatusCode> {
    let client = settings.get_client();

    let resource_name = settings.backend_to_slab_name(&backend_id);
    let mut events = past_events(client, &resource_name, &settings.namespace)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    events.sort_by_key(|d| d.event_time.clone());
    let last_event = events.last().ok_or(StatusCode::NO_CONTENT)?;

    Ok(Json(event_to_json(last_event)))
}

async fn status_handler(
    Path((backend_id,)): Path<(String,)>,
    Extension(settings): Extension<Arc<ApiSettings>>,
) -> Result<Sse<impl Stream<Item = Result<AxumSseEvent, BoxError>>>, StatusCode> {
    let client = settings.get_client();

    let name = format!("{}{}", settings.service_prefix, backend_id);
    let events = event_stream(client, &name, &settings.namespace)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let sse_events: _ = convert_stream(events).into_stream();

    Ok(Sse::new(sse_events))
}

pub struct ApiSettings {
    pub namespace: String,
    pub url_template: Option<String>,
    pub api_server_base: Option<String>,
    pub service_prefix: String,
    pub client: Client,
}

impl ApiSettings {
    pub fn get_client(&self) -> Client {
        self.client.clone()
    }

    pub fn backend_to_slab_name(&self, backend_id: &str) -> String {
        format!("{}{}", self.service_prefix, backend_id)
    }

    pub fn backend_to_url(&self, backend_id: &str) -> Option<String> {
        self.url_template
            .as_ref()
            .map(|d| d.replace("{}", &backend_id))
    }

    pub fn backend_api_path(&self, backend_id: &str, path: &str) -> Option<String> {
        let api_server_base = self.api_server_base.as_ref()?;

        Some(format!(
            "{}/backend/{}/{}",
            api_server_base, backend_id, path
        ))
    }

    pub fn get_init_result(&self, backend_id: &str) -> SpawnResult {
        let ready_url = self.backend_api_path(&backend_id, "ready");
        let status_url = self.backend_api_path(&backend_id, "status");

        SpawnResult {
            url: self.backend_to_url(backend_id),
            name: backend_id.to_string(),
            ready_url,
            status_url,
        }
    }

    pub fn slab_name_to_backend(&self, slab_name: &str) -> Option<String> {
        slab_name
            .strip_prefix(&self.service_prefix)
            .map(|t| t.to_string())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnResult {
    pub url: Option<String>,
    pub name: String,
    pub ready_url: Option<String>,
    pub status_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnPayload {
    /// The container image used to create the container.
    image: String,

    /// HTTP port to expose on the container.
    port: Option<u16>,

    /// Environment variables to expose on the container.
    #[serde(default)]
    env: HashMap<String, String>,

    /// Duration of time (in seconds) before the pod is shut down.
    grace_period_seconds: Option<u32>,
}

pub async fn spawn_handler(
    Json(payload): Json<SpawnPayload>,
    Extension(settings): Extension<Arc<ApiSettings>>,
) -> Result<Json<SpawnResult>, StatusCode> {
    let slab = SessionLivedBackendBuilder::new(&payload.image)
        .with_env(payload.env)
        .with_port(payload.port)
        .with_grace_period(payload.grace_period_seconds)
        .build_prefixed(&settings.service_prefix);

    let client = Client::try_default().await.map_err(|error| {
        tracing::error!(%error, "Error getting client");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let api = Api::<SessionLivedBackend>::namespaced(client, &settings.namespace);

    let result = api
        .create(
            &PostParams {
                field_manager: Some(SPAWNER_GROUP.to_string()),
                ..PostParams::default()
            },
            &slab,
        )
        .await
        .map_err(|error| {
            tracing::error!(%error, "Error creating SessionLivedBackend.");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let prefixed_name = result.name();
    let name = settings
        .slab_name_to_backend(&prefixed_name)
        .ok_or_else(|| {
            tracing::warn!("Couldn't strip prefix from name.");
            StatusCode::EXPECTATION_FAILED
        })?;

    let url = settings.backend_to_url(&name);

    tracing::info!(?url, %name, "Created SessionLivedBackend.");

    Ok(Json(settings.get_init_result(&name)))
}
