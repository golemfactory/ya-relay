pub(crate) mod tcp_registry;
pub mod transport_sender;
mod virtual_layer;

use anyhow::bail;
use futures::StreamExt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_core::server_session::TransportType;
use ya_relay_core::NodeId;
use ya_relay_stack::Channel;

use self::tcp_registry::ChannelType;
use self::virtual_layer::TcpLayer;
use crate::client::{ClientConfig, ForwardSender, Forwarded, GenericSender};
use crate::session::SessionLayer;

/// TODO: Consider using bounded channel. Tcp could have impression that we are receiving
///       messages, despite we are only putting them into channel.
pub type ForwardReceiver = tokio::sync::mpsc::UnboundedReceiver<Forwarded>;

/// Responsible for sending data. Handles different kinds of transport types:
/// - Unreliable [`TransportLayer::forward_unreliable`] - send raw packets without any delivery
///   guarantees. It is equivalent of using UDP.
/// - Reliable [`TransportLayer::forward_reliable`] - send messages with TCP delivery and ordering
///   guarantees.
/// - Transfer [`TransportLayer::forward_transfer`] - uses the same transport as reliable protocol,
///   but should be used for heavier transfers. Packets are sent using separate channel, what helps
///   with avoiding blocking more important messages in sending queue.
#[derive(Clone)]
pub struct TransportLayer {
    pub config: Arc<ClientConfig>,

    pub session_layer: SessionLayer,
    pub virtual_tcp: TcpLayer,

    state: Arc<RwLock<TransportLayerState>>,

    /// Shared channel with TcpLayer for sending processed packets to external layers.
    ingress_channel: Channel<Forwarded>,
}

#[derive(Default)]
struct TransportLayerState {
    /// Every default and secondary NodeId has separate entry here.
    forward_unreliable: HashMap<NodeId, ForwardSender>,
    forward_transfer: HashMap<NodeId, ForwardSender>,
    forward_reliable: HashMap<NodeId, ForwardSender>,
}

impl TransportLayer {
    pub fn new(config: Arc<ClientConfig>) -> TransportLayer {
        let out = Channel::<Forwarded>::default();
        let session_layer = SessionLayer::new(config.clone());
        let virtual_tcp = TcpLayer::new(
            &config.node_pub_key,
            &config.stack_config,
            &out,
            session_layer.clone(),
        );

        TransportLayer {
            config,
            session_layer,
            virtual_tcp,
            state: Default::default(),
            ingress_channel: out,
        }
    }

    pub(crate) async fn spawn(&mut self) -> anyhow::Result<SocketAddr> {
        let bind_addr = self.session_layer.spawn().await?;
        self.virtual_tcp
            .spawn(self.session_layer.config.node_id)
            .await?;

        self.spawn_ingress_handler().await?;
        Ok(bind_addr)
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        let channels = {
            let mut state = self.state.write().await;

            let unreliable = state.forward_unreliable.drain().collect::<Vec<_>>();
            let transfer = state.forward_transfer.drain().collect::<Vec<_>>();
            let reliable = state.forward_reliable.drain().collect::<Vec<_>>();

            unreliable
                .into_iter()
                .chain(transfer.into_iter())
                .chain(reliable.into_iter())
                .collect::<Vec<_>>()
        };
        for (_, mut channel) in channels {
            channel.disconnect().await.ok();
        }

        self.virtual_tcp
            .shutdown(self.session_layer.config.node_id)
            .await;

        // After Tcp shutdown will return, we are sending last Tcp packet to notify other Node,
        // that connection is closed. We shouldn't close sessions before we give them chance to be sent.
        tokio::time::sleep(Duration::from_millis(100)).await;

        self.session_layer.shutdown().await
    }

    pub fn forward_receiver(&self) -> Option<ForwardReceiver> {
        self.ingress_channel.receiver()
    }

    async fn dispatch(&self, packet: Forwarded) {
        log::trace!(
            "[TransportLayer] Dispatching packet from [{}] ({})",
            packet.node_id,
            packet.transport
        );

        match &packet.transport {
            TransportType::Unreliable => self.dispatch_unreliable(packet).await,
            TransportType::Reliable => self.virtual_tcp.dispatch(packet).await,
            // Currently `SessionLayer` responds only with `Unreliable` and `Reliable`, because only TcpLayer
            // can distinguish packets between `Reliable` and `Transfer`.
            // Nevertheless this function will work correctly even when getting `Transfer` variant.
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

    async fn get_forward_channel(
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

    pub async fn forward_reliable(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        self.forward_virtual_tcp(node_id, TransportType::Reliable)
            .await
    }

    pub async fn forward_transfer(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        self.forward_virtual_tcp(node_id, TransportType::Transfer)
            .await
    }

    /// NodeId can be either default or secondary.
    /// TODO: Make this function resistant to dropping future
    pub async fn forward_virtual_tcp(
        &self,
        node_id: NodeId,
        channel: TransportType,
    ) -> anyhow::Result<ForwardSender> {
        log::trace!("[forward_virtual_tcp]: node_id: {}, channel: {:?}", node_id, channel);
        match self.get_forward_channel(node_id, channel).await {
            // If connection was closed in the meantime, it will be initialized on demand.
            // It will be problematic in some cases, because this can last up to a few seconds.
            // In worst case scenario initialization will fail and we will wait 5s until timeout.
            // Since user uses channel, sending will return immediately after item will be taken from
            // queue, so he won't find out, but the response he expects won't come.
            // This is argument for changing channels API to `TcpSender`.
            Some(tx) => {
                log::trace!("[forward_virtual_tcp]: forward channel already exists.");
                Ok(tx)
            },
            None => {
                log::trace!("[forward_virtual_tcp]: forward channel does not exist.");
                // Check if this isn't secondary identity. TcpLayer should always get default id.
                // TODO: Consider how to handle changing identities.
                // TODO: Maybe we should call `self.session_layer::session` and pass it to `connect`.
                let info = self.session_layer.query_node_info(node_id).await?;

                if let Some(tx) = self
                    .get_forward_channel(info.default_node_id(), channel)
                    .await
                {
                    self.set_forward_channel(node_id, channel, tx.clone()).await;
                    return Ok(tx);
                }

                let channel_port = match channel {
                    TransportType::Reliable => ChannelType::Messages,
                    TransportType::Transfer => ChannelType::Transfer,
                    _ => bail!("Programming error: `forward_generic` shouldn't been used for unreliable connection.")
                };

                let sender: ForwardSender = self
                    .virtual_tcp
                    .connect(info.default_node_id(), channel_port)
                    .await?
                    .into();

                self.set_forward_channel(node_id, channel, sender.clone())
                    .await;
                log::trace!("[forward_virtual_tcp]: forward channel created.");
                Ok(sender)
            }
        }
    }

    /// NodeId can be either default or secondary.
    /// TODO: Make this function resistant to dropping future
    pub async fn forward_unreliable(&self, node_id: NodeId) -> anyhow::Result<ForwardSender> {
        // This will return fast, if we already have this channel.
        // These lines are not necessary, because code below would do the job,
        // but this way we avoid querying write lock and asking session layer for `RoutingSender`
        // on every attempt to send message.
        if let Some(tx) = self
            .get_forward_channel(node_id, TransportType::Unreliable)
            .await
        {
            return Ok(tx);
        }

        let routing: ForwardSender = self.session_layer.session(node_id).await?.into();

        let routing = {
            let mut state = self.state.write().await;
            match state.forward_unreliable.get(&node_id) {
                Some(tx) => return Ok(tx.clone()),
                None => {
                    state.forward_unreliable.insert(node_id, routing.clone());
                    routing
                }
            }
        };

        Ok(routing)
    }
}
