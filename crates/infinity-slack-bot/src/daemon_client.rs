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

    pub async fn create_session(
        &self,
        cwd: PathBuf,
        model: Option<infinity_protocol::ModelRef>,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::CreateSession {
                cwd,
                location: None,
                model,
                keeps_session_alive: false,
            })
            .await?;
        Ok(())
    }

    /// Send user input addressed to the given agent thread (the session root
    /// or a subagent thread).
    pub async fn send_input(
        &self,
        thread_id: &infinity_protocol::ThreadRef,
        text: &str,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::UserInput {
                thread_id: thread_id.clone(),
                text: text.to_owned(),
            })
            .await?;
        Ok(())
    }

    /// Interrupt in-flight generation on the given agent thread (and its
    /// descendant subtree) without queuing any input.
    pub async fn interrupt(
        &self,
        thread_id: &infinity_protocol::ThreadRef,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::Interrupt {
                thread_id: thread_id.clone(),
            })
            .await?;
        Ok(())
    }

    /// Fork session `thread_id` into a new session just before its
    /// `user_turn`-th user turn (see [`ClientMessage::ForkSession`]). This
    /// connection is attached to the fork, which replies `Connected`.
    pub async fn fork_session(
        &self,
        thread_id: &infinity_protocol::ThreadRef,
        user_turn: usize,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::ForkSession {
                thread_id: thread_id.clone(),
                user_turn,
                keeps_session_alive: false,
            })
            .await?;
        Ok(())
    }

    /// Switch `thread_id`'s model (see [`ClientMessage::SwitchModel`] —
    /// per-thread, does not propagate to child threads).
    pub async fn switch_model(
        &self,
        thread_id: &infinity_protocol::ThreadRef,
        model: &infinity_protocol::ModelRef,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::SwitchModel {
                thread_id: thread_id.clone(),
                model: model.clone(),
            })
            .await?;
        Ok(())
    }

    /// Trigger context compaction for the session rooted at `session_id`    /// (see [`ClientMessage::TriggerCompaction`]).
    pub async fn trigger_compaction(
        &self,
        session_id: &infinity_protocol::ThreadRef,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::TriggerCompaction {
                root_thread_id: session_id.clone(),
            })
            .await?;
        Ok(())
    }

    pub async fn connect_session(
        &self,
        session_id: &infinity_protocol::ThreadRef,
        thread_id: Option<infinity_protocol::ThreadRef>,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::Connect {
                root_thread_id: session_id.clone(),
                thread_id,
                keeps_session_alive: false,
            })
            .await?;
        Ok(())
    }

    pub async fn answer_choice(
        &self,
        choice_id: &infinity_protocol::ChoiceId<str>,
        selected: usize,
    ) -> Result<(), BoxError> {
        self.tx
            .send(ClientMessage::UserChoiceAnswered {
                choice_id: choice_id.to_owned(),
                selected,
            })
            .await?;
        Ok(())
    }
}
