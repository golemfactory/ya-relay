use crate::server::IpChecker;
use crate::udp_server::UdpSocket;
use crate::SessionManager;
use anyhow::{anyhow, bail, Context};
use parking_lot::Mutex;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio::task::spawn_local;
use ya_relay_core::server_session::SessionId;
use ya_relay_proto::proto::{control, packet, Control, Message, Packet};

enum ControlCommand {
    DropSession {
        session_id: SessionId,
        silent: bool,
    },
    CheckIp {
        session_id: SessionId,
        reply_tx: oneshot::Sender<bool>,
    },
}

#[derive(Clone)]
pub struct ServerControl {
    tx: mpsc::Sender<ControlCommand>,
}

impl ServerControl {
    pub async fn disconnect(&self, session_id: SessionId, silent: bool) -> anyhow::Result<()> {
        if let Err(_e) = self
            .tx
            .send(ControlCommand::DropSession { session_id, silent })
            .await
        {
            bail!("failed to send disconnect command");
        }
        Ok(())
    }

    pub async fn check_ip(&self, session_id: SessionId) -> anyhow::Result<bool> {
        let (reply_tx, rx) = oneshot::channel();
        self.tx
            .send(ControlCommand::CheckIp {
                session_id,
                reply_tx,
            })
            .await
            .map_err(|_e| anyhow!("failed to check ip"))?;
        let v = rx.await.with_context(|| "failed to recv check ip result")?;
        Ok(v)
    }
}

#[derive(Clone)]
pub struct Handle {
    rx: Arc<Mutex<Option<mpsc::Receiver<ControlCommand>>>>,
}

impl Handle {
    pub(crate) fn spawn(
        &self,
        sm: &Arc<SessionManager>,
        socket: &Rc<UdpSocket>,
        ip_checker: &Rc<IpChecker>,
    ) {
        if let Some(mut rx) = self.rx.lock().take() {
            let sm_ref = Arc::downgrade(sm);
            let socket_ref = Rc::downgrade(socket);
            let ip_checker = ip_checker.clone();
            let _handle = spawn_local(async move {
                while let Some(command) = rx.recv().await {
                    let sm = sm_ref.upgrade()?;
                    let socket = socket_ref.upgrade()?;
                    match command {
                        ControlCommand::DropSession { session_id, silent } => {
                            if let Some(session_ref) = sm.remove_session(&session_id) {
                                let peer = session_ref.peer;
                                drop(session_ref);

                                if !silent {
                                    let p = Packet {
                                        session_id: session_id.to_vec(),
                                        kind: Some(packet::Kind::Control(Control {
                                            kind: Some(control::Kind::Disconnected(
                                                control::Disconnected {
                                                    by: Some(control::disconnected::By::SessionId(
                                                        session_id.to_vec(),
                                                    )),
                                                },
                                            )),
                                        })),
                                    };
                                    let b = p.encode_to_vec();
                                    socket.send_to(&b, peer).await.ok();
                                    log::info!("[{:?}] send drop session {}", peer, session_id)
                                }
                            }
                        }
                        ControlCommand::CheckIp {
                            session_id,
                            reply_tx,
                        } => {
                            log::info!("check ip for [{}]", session_id);
                            if let Some(s) = sm.session(&session_id) {
                                let _ = ip_checker.check_ip_status(
                                    Instant::now(),
                                    s,
                                    |result, _session_ref| {
                                        reply_tx.send(result).ok();
                                    },
                                );
                            } else {
                                log::error!("session not found: {}", session_id);
                                reply_tx.send(false).ok();
                            }
                        }
                    }
                }
                Some(())
            });
        }
    }
}

pub(crate) fn create() -> (ServerControl, Handle) {
    let (tx, rx) = mpsc::channel(1);
    let control = ServerControl { tx };
    let handle = Handle {
        rx: Arc::new(Mutex::new(Some(rx))),
    };

    (control, handle)
}
