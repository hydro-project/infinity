use std::path::PathBuf;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use infinity_protocol::{ClientMessage, DaemonMessage};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::BoxError;

pub struct DaemonClient {
    pub tx: mpsc::Sender<ClientMessage>,
    pub rx: mpsc::Receiver<DaemonMessage>,
}

impl DaemonClient {
    pub async fn connect() -> Result<Self, BoxError> {
        let sock_path = infinity_protocol::socket_path();
        let stream = UnixStream::connect(&sock_path).await.map_err(|e| {
            format!(
                "Cannot connect to daemon at {}: {e}\nIs `infinity` running?",
                sock_path.display()
            )
        })?;

        let mut framed = Framed::new(stream, LengthDelimitedCodec::new());

        let (client_tx, mut client_rx) = mpsc::channel::<ClientMessage>(64);
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonMessage>(256);

        // Bridge: forward ClientMessages to socket, DaemonMessages from socket
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = client_rx.recv() => {
                        let json = serde_json::to_vec(&msg).expect("serialize ClientMessage");
                        if framed.send(Bytes::from(json)).await.is_err() {
                            break;
                        }
                    }
                    frame = framed.next() => {
                        match frame {
                            Some(Ok(data)) => {
                                if let Ok(msg) = serde_json::from_slice::<DaemonMessage>(&data) {
                                    if daemon_tx.send(msg).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            _ => break,
                        }
                    }
                }
            }
        });

        Ok(Self {
            tx: client_tx,
            rx: daemon_rx,
        })
    }

    pub async fn create_session(&self, cwd: PathBuf) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::CreateSession {
                cwd,
                location: None,
                model: None,
            })
            .await?;
        Ok(())
    }

    pub async fn send_input(&self, session_id: &str, text: &str) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::UserInput {
                session_id: session_id.to_owned(),
                text: text.to_owned(),
            })
            .await?;
        Ok(())
    }

    pub async fn connect_session(
        &self,
        session_id: &str,
        thread_id: Option<String>,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::Connect {
                session_id: session_id.to_owned(),
                thread_id,
            })
            .await?;
        Ok(())
    }

    pub async fn answer_choice(&self, choice_id: &str, selected: usize) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::UserChoiceAnswered {
                choice_id: choice_id.to_owned(),
                selected,
            })
            .await?;
        Ok(())
    }
}
