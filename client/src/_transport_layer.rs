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

use crate::_client::{ClientConfig, Forwarded};
use crate::_session_layer::SessionLayer;
use crate::_tcp_registry::ChannelType;
use crate::_transport_sender::{ForwardSender, GenericSender};
use crate::_virtual_layer::TcpLayer;

pub type ForwardReceiver = tokio::sync::mpsc::UnboundedReceiver<Forwarded>;

/// Responsible for sending data. Handles different kinds of transport types.
#[derive(Clone)]
pub struct TransportLayer {
    pub config: Arc<ClientConfig>,

    pub session_layer: SessionLayer,
    pub virtual_tcp: TcpLayer,

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
            state: Arc::new(RwLock::new(TransportLayerState {
                forward_unreliable: Default::default(),
                forward_transfer: Default::default(),
                forward_reliable: Default::default(),
            })),
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

            let channels1 = state.forward_unreliable.drain().collect::<Vec<_>>();
            let channels2 = state.forward_transfer.drain().collect::<Vec<_>>();
            let channels3 = state.forward_reliable.drain().collect::<Vec<_>>();

            channels1
                .into_iter()
                .chain(channels2.into_iter())
                .chain(channels3.into_iter())
                .collect::<Vec<_>>()
        };
        for (_, mut channel) in channels {
            channel.disconnect().await.ok();
        }

        self.virtual_tcp.shutdown().await;

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
    /// TODO: Make this function resistant to dropping future
    pub async fn forward_generic(
        &self,
        node_id: NodeId,
        channel: TransportType,
    ) -> anyhow::Result<ForwardSender> {
        match self.forward_channel(node_id, channel).await {
            // If connection was closed in the meantime, it will be initialized on demand.
            // It will be problematic in some cases, because this can last up to a few seconds.
            // In worst case scenario initialization will fail and we will wait 5s until timeout.
            // Since user uses channel, sending will return immediately after item will be taken from
            // queue, so he won't find out, but the response he expects won't come.
            // This is argument for changing channels API to `TcpSender`.
            Some(tx) => Ok(tx),
            None => {
                // Check if this isn't secondary identity. TcpLayer should always get default id.
                // TODO: Consider how to handle changing identities.
                let info = self.session_layer.query_node_info(node_id).await?;

                if let Some(tx) = self.forward_channel(info.node_id(), channel).await {
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
                    .connect(info.node_id(), channel_port)
                    .await?
                    .into();

                self.set_forward_channel(node_id, channel, sender.clone())
                    .await;
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
            .forward_channel(node_id, TransportType::Unreliable)
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
