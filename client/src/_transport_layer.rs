use anyhow::bail;
use futures::channel::mpsc;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_core::session::TransportType;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{Forward, Payload};
use ya_relay_stack::{Channel, Connection};

use crate::_routing_session::RoutingSender;
use crate::_session_layer::SessionLayer;
use crate::_virtual_layer::{PortType, TcpLayer};
use crate::client::{ClientConfig, ForwardSender, Forwarded};
use crate::ForwardReceiver;

/// Responsible for sending data. Handles different kinds of transport types.
#[derive(Clone)]
struct TransportLayer {
    pub config: Arc<ClientConfig>,

    session_layer: SessionLayer,
    virtual_tcp: TcpLayer,

    state: Arc<RwLock<TransportLayerState>>,

    /// Shared channel with TcpLayer for sending processed packets to external layers.
    ingress_channel: Channel<Forwarded>,
}

struct TransportLayerState {
    /// Every default and secondary NodeId has separate entry here.
    forward_unreliable: HashMap<NodeId, ForwardSender>,
    forward_transfer: HashMap<NodeId, ForwardSender>,
    forward_reliable: HashMap<NodeId, ForwardSender>,
}

impl TransportLayer {
    async fn dispatch(&self, packet: Forwarded) {
        match &packet.transport {
            TransportType::Unreliable => self.dispatch_unreliable(packet).await,
            TransportType::Reliable => self.virtual_tcp.dispatch(packet).await,
            // We shouldn't get this transport type from `SessionLayer`, because only TcpLayer
            // can distinguish packets between `Reliable` and `Transfer`.
            TransportType::Transfer => self.virtual_tcp.dispatch(packet).await,
        }
    }

    pub async fn dispatch_unreliable(&self, forward: Forwarded) {
        self.ingress_channel.tx.send(forward).ok();
    }

    async fn spawn_ingress_handler(&self) -> anyhow::Result<()> {
        let ingress_rx = self
            .session_layer
            .receiver()
            .ok_or_else(|| anyhow::anyhow!("Ingress traffic receiver already spawned"))?;

        tokio::task::spawn_local(self.clone().ingress_handler(ingress_rx));
        Ok(())
    }

    async fn ingress_handler(self, ingress_rx: ForwardReceiver) {
        UnboundedReceiverStream::new(ingress_rx)
            .for_each(move |forwarded| {
                let myself = self.clone();
                async move {
                    myself.dispatch(forwarded).await;
                }
            })
            .await
    }

    async fn forward_channel(
        &self,
        node_id: NodeId,
        channel: TransportType,
    ) -> Option<ForwardSender> {
        let state = self.state.read().await;
        match channel {
            TransportType::Reliable => state.forward_reliable.get(&node_id).cloned(),
            TransportType::Transfer => state.forward_transfer.get(&node_id).cloned(),
            TransportType::Unreliable => state.forward_unreliable.get(&node_id).cloned(),
        }
    }

    async fn set_forward_channel(
        &self,
        node_id: NodeId,
        channel: TransportType,
        tx: ForwardSender,
    ) {
        let mut state = self.state.write().await;
        match channel {
            TransportType::Reliable => state.forward_reliable.insert(node_id, tx),
            TransportType::Transfer => state.forward_transfer.insert(node_id, tx),
            TransportType::Unreliable => state.forward_unreliable.insert(node_id, tx),
        };
    }

    pub async fn forward(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        self.forward_generic(node_id, TransportType::Reliable).await
    }

    pub async fn forward_transfer(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        self.forward_generic(node_id, TransportType::Transfer).await
    }

    /// NodeId can be either default or secondary.
    pub async fn forward_generic(
        &self,
        node_id: NodeId,
        channel: TransportType,
    ) -> anyhow::Result<ForwardSender> {
        unimplemented!();
        // let node = self.get_node(node_id).await?;
        // let tx = {
        //     match self.forward_channel(node_id, channel).await {
        //         Some(tx) => tx,
        //         None => {
        //             self.virtual_tcp_fast_lane.borrow_mut().clear();
        //
        //             let (_guard, was_locked) = node.guard().await;
        //
        //             if was_locked {
        //                 // If we still don't have this channel, probably it was connected on other
        //                 // `TransportType`, so we should try to make new connection anyway.
        //                 if let Some(tx) = self.forward_channel(node_id, channel).await {
        //                     return Ok(tx);
        //                 }
        //             }
        //
        //             let channel_port = match channel {
        //                 TransportType::Reliable => PortType::Messages,
        //                 TransportType::Transfer => PortType::Transfer,
        //                 _ => bail!("Programming error: `forward_generic` shouldn't been used for unreliable connection.")
        //             };
        //
        //             let conn = self.virtual_tcp.connect(node.clone(), channel_port).await?;
        //             let (tx, rx) = mpsc::channel(1);
        //             tokio::task::spawn_local(self.clone().forward_reliable_handler(conn, node, rx));
        //
        //             self.set_forward_channel(node_id, channel, tx.clone()).await;
        //             tx
        //         }
        //     }
        // };
        //
        // Ok(tx)
    }

    /// NodeId can be either default or secondary.
    pub async fn forward_unreliable(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        // This will return fast, if we already have this channel.
        // These lines are not necessary, because code below would do the job,
        // but this way we avoid querying write lock and asking session layer for `RoutingSender`
        // on every attempt to send message.
        if let Some(tx) = self
            .forward_channel(node_id, TransportType::Unreliable)
            .await
        {
            return Ok(tx);
        }

        let routing = self.session_layer.session(node_id).await?;

        let (tx, rx) = {
            let mut state = self.state.write().await;
            match state.forward_unreliable.get(&node_id) {
                Some(tx) => return Ok(tx.clone()),
                None => {
                    let (tx, rx) = mpsc::channel(1);
                    state.forward_unreliable.insert(node_id, tx.clone());
                    (tx, rx)
                }
            }
        };

        tokio::task::spawn_local(self.clone().forward_unreliable_handler(routing, rx));
        Ok(tx)
    }

    // async fn forward_reliable_handler(
    //     self,
    //     connection: Connection,
    //     node: NodeEntry,
    //     mut rx: mpsc::Receiver<Payload>,
    // ) {
    //     let pause = node.session.forward_pause.clone();
    //     let session = node.session.clone();
    //
    //     while let Some(payload) = self.virtual_tcp.get_next_fwd_payload(&mut rx, &pause).await {
    //         log::trace!(
    //             "Forwarding message to {} through {} (session id: {})",
    //             node.id,
    //             session.remote,
    //             session.id
    //         );
    //
    //         if let Err(err) = self.virtual_tcp.send(payload, connection).await {
    //             log::debug!(
    //                 "[{}] forward to {} through {} (session id: {}) failed: {err}",
    //                 self.config.node_id,
    //                 node.id,
    //                 session.remote,
    //                 session.id,
    //             );
    //             break;
    //         }
    //     }
    //
    //     log::debug!(
    //         "[{}] forward: disconnected from: {}",
    //         node.id,
    //         session.remote
    //     );
    //
    //     rx.close();
    //     let _ = self.close_session(node.session.clone()).await;
    // }

    async fn forward_unreliable_handler(
        self,
        mut session: RoutingSender,
        mut rx: mpsc::Receiver<Payload>,
    ) {
        unimplemented!();
        // // TODO: Fix pause forwarding. Maybe it should be moved to `RoutingSender` internals,
        // //       since pausing is set per `DirectSession`.
        // let pause = node.session.forward_pause.clone();
        //
        // while let Some(payload) = self.virtual_tcp.get_next_fwd_payload(&mut rx, &pause).await {
        //     // TODO: Session might be established or chosen after we call `RoutingSender::send`,
        //     //       so this log message has to be fixed.
        //     log::trace!(
        //         "Forwarding message (U) to {} through {} (session id: {})",
        //         session.target(),
        //         session.route(),
        //         session.session_id()
        //     );
        //
        //     if let Err(error) = session
        //         .send(payload.into(), TransportType::Unreliable)
        //         .await
        //     {
        //         log::debug!(
        //             "[{}] forward (U) to {} through {} (session id: {}) failed: {error}",
        //             self.config.node_id,
        //             session.target(),
        //             session.route(),
        //             session.session_id()
        //         );
        //         break;
        //     }
        // }
        //
        // log::debug!(
        //     "[{}] forward (U): disconnected from: {}",
        //     node.id,
        //     session.target()
        // );
        //
        // rx.close();
        // self.remove_node(node.id).await;
    }
}
