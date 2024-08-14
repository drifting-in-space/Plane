use super::{SocketPath, WrappedMessage};
use crate::util::GuardHandle;
use anyhow::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{fmt::Debug, fs, path::Path, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};

/// A server for handling Unix socket connections using typed messages.
#[derive(Clone)]
pub struct TypedUnixSocketServer<MessageToServer, MessageToClient>
where
    MessageToServer: Send + Sync + 'static + Clone + Debug + Serialize + for<'de> Deserialize<'de>,
    MessageToClient: Send + Sync + 'static + Clone + Debug + Serialize + for<'de> Deserialize<'de>,
{
    event_tx: broadcast::Sender<MessageToServer>,
    request_tx: broadcast::Sender<WrappedMessage<MessageToServer>>,
    response_tx: broadcast::Sender<WrappedMessage<MessageToClient>>,
    _socket_path: Arc<SocketPath>,
    _loop_task: Arc<GuardHandle>,
}

impl<MessageToServer, MessageToClient> TypedUnixSocketServer<MessageToServer, MessageToClient>
where
    MessageToServer: Send + Sync + 'static + Clone + Debug + Serialize + for<'de> Deserialize<'de>,
    MessageToClient: Send + Sync + 'static + Clone + Debug + Serialize + for<'de> Deserialize<'de>,
{
    /// Creates a new `TypedUnixSocketServer` and binds to the specified Unix socket path.
    pub async fn new<P: AsRef<Path>>(socket_path: P) -> Result<Self, Error> {
        let socket_path = socket_path.as_ref().to_path_buf();
        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }
        let listener = UnixListener::bind(&socket_path)?;
        let (event_tx, _) = broadcast::channel(100);
        let (request_tx, _) = broadcast::channel(100);
        let (response_tx, _) = broadcast::channel(100);

        let loop_task = {
            let event_tx = event_tx.clone();
            let request_tx = request_tx.clone();
            let response_tx = response_tx.clone();
            let response_rx = response_tx.subscribe(); // ensure we subscribe synchronously to avoid issues sending messages
            GuardHandle::new(async move {
                let mut response_rx = response_rx; // we're doing this so that we can re-subscribe at the end of the loop for successive iterations
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            if handle_connection(
                                stream,
                                event_tx.clone(),
                                request_tx.clone(),
                                response_rx,
                            )
                            .await
                            .is_ok()
                            {
                                tracing::info!("Shutdown server");
                                break;
                            }
                            tracing::error!("Error handling connection");
                        }
                        Err(e) => {
                            tracing::error!(%e, "Error accepting connection.");
                        }
                    }
                    response_rx = response_tx.subscribe();
                }
            })
        };

        Ok(Self {
            event_tx,
            request_tx,
            response_tx,
            _socket_path: Arc::new(SocketPath(socket_path)),
            _loop_task: Arc::new(loop_task),
        })
    }

    /// Subscribes to events from clients.
    pub fn subscribe_events(&self) -> broadcast::Receiver<MessageToServer> {
        self.event_tx.subscribe()
    }

    /// Subscribes to requests from clients.
    pub fn subscribe_requests(&self) -> broadcast::Receiver<WrappedMessage<MessageToServer>> {
        self.request_tx.subscribe()
    }

    /// Sends a response to a client's request.
    pub async fn send_response(
        &self,
        request: &WrappedMessage<MessageToServer>,
        response: MessageToClient,
    ) -> Result<(), Error> {
        let response = WrappedMessage {
            id: request.id.clone(),
            message: response,
        };
        self.response_tx.send(response)?;
        Ok(())
    }

    /// Sends a message to the client without waiting for a response.
    pub async fn send_message(&self, message: MessageToClient) -> Result<(), Error> {
        let message_msg = WrappedMessage { id: None, message };
        self.response_tx.send(message_msg)?;
        Ok(())
    }
}

async fn handle_connection<MessageToServer, MessageToClient>(
    stream: UnixStream,
    event_tx: broadcast::Sender<MessageToServer>,
    request_tx: broadcast::Sender<WrappedMessage<MessageToServer>>,
    mut response_rx: broadcast::Receiver<WrappedMessage<MessageToClient>>,
) -> Result<(), anyhow::Error>
where
    MessageToServer: Send + Sync + 'static + Clone + Debug + Serialize + for<'de> Deserialize<'de>,
    MessageToClient: Send + Sync + 'static + Clone + Debug + Serialize + for<'de> Deserialize<'de>,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let reader = BufReader::new(read_half);
    let writer = BufWriter::new(write_half);

    let mut lines = reader.lines();
    let mut writer = writer;

    // Task to handle receiving messages
    let recv_task = {
        let event_tx = event_tx.clone();
        async move {
            loop {
                let result = lines.next_line().await;
                match result {
                    Ok(Some(line)) => {
                        let msg: WrappedMessage<MessageToServer> = match serde_json::from_str(&line)
                        {
                            Ok(msg) => msg,
                            Err(e) => {
                                tracing::error!(%e, ?line, "Error deserializing message.");
                                continue;
                            }
                        };
                        match msg {
                            WrappedMessage { id: Some(_), .. } => {
                                if let Err(e) = request_tx.send(msg.clone()) {
                                    tracing::error!(%e, ?msg, "Error sending request.");
                                }
                            }
                            WrappedMessage { id: None, message } => {
                                if let Err(e) = event_tx.send(message.clone()) {
                                    tracing::error!(%e, msg = ?message, "Error sending event.");
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::info!("Connection closed by client");
                        return Err::<(), anyhow::Error>(anyhow::anyhow!(
                            "Connection closed by server"
                        ));
                    }
                    Err(e) => {
                        tracing::error!(%e, "Error reading line.");
                        return Err(anyhow::anyhow!("Error reading line: {}", e));
                    }
                }
            }
        }
    };

    // Task to handle sending responses
    let send_task = {
        async move {
            loop {
                let result = response_rx.recv().await;
                match result {
                    Ok(response) => {
                        let response_str = match serde_json::to_string(&response) {
                            Ok(response_str) => response_str,
                            Err(e) => {
                                tracing::error!(%e, ?response, "Error serializing response.");
                                continue;
                            }
                        };
                        if let Err(e) = writer.write_all(response_str.as_bytes()).await {
                            tracing::error!(%e, ?response, "Error writing response.");
                        }
                        if let Err(e) = writer.write_all(b"\n").await {
                            tracing::error!(%e, ?response, "Error writing newline.");
                        }
                        if let Err(e) = writer.flush().await {
                            tracing::error!(%e, ?response, "Error flushing writer.");
                        }
                    }
                    Err(e) => {
                        tracing::error!(%e, "Error receiving response.");
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok(())
        }
    };

    tokio::try_join!(recv_task, send_task)?;

    Ok(())
}
