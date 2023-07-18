use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;
use url::Url;

use ya_relay_core::crypto::{CryptoProvider, FallbackCryptoProvider, PublicKey};
use ya_relay_core::error::InternalError;
use ya_relay_core::udp_stream::resolve_max_payload_overhead_size;
use ya_relay_core::utils::parse_udp_url;
use ya_relay_core::NodeId;
use ya_relay_proto::proto::{Forward, MAX_TAG_SIZE};
use ya_relay_stack::StackConfig;

use crate::client::Client;
use crate::session::network_view::NetworkViewConfig;

#[derive(Clone, Copy)]
pub enum FailFast {
    Yes,
    No,
}

#[derive(Clone)]
pub struct ClientConfig {
    pub node_id: NodeId,
    pub node_pub_key: PublicKey,
    pub crypto: Rc<dyn CryptoProvider>,
    pub challenge_difficulty: u64,

    pub bind_url: Url,
    pub srv_addr: SocketAddr,
    pub auto_connect: bool,
    pub auto_connect_fail_fast: bool,
    pub session_expiration: Duration,
    pub stack_config: StackConfig,
    pub ping_measure_interval: Duration,

    pub session_request_timeout: Duration,
    pub challenge_request_timeout: Duration,

    pub reverse_connection_tmp_timeout: Duration,
    pub reverse_connection_real_timeout: Duration,
    pub incoming_session_timeout: Duration,
    pub neighbourhood_ttl: Duration,
    pub registry_config: NetworkViewConfig,
}

pub struct ClientBuilder {
    bind_url: Option<Url>,
    srv_url: Url,
    crypto: Option<Rc<dyn CryptoProvider>>,
    auto_connect: bool,
    auto_connect_fail_fast: bool,
    session_expiration: Option<Duration>,
    stack_config: StackConfig,
}

impl ClientBuilder {
    pub fn from_url(url: Url) -> ClientBuilder {
        ClientBuilder {
            bind_url: None,
            srv_url: url,
            crypto: None,
            auto_connect: false,
            auto_connect_fail_fast: false,
            session_expiration: None,
            stack_config: Default::default(),
        }
    }

    pub fn crypto(mut self, provider: impl CryptoProvider + 'static) -> ClientBuilder {
        self.crypto = Some(Rc::new(provider));
        self
    }

    pub fn connect(mut self, fail_fast: FailFast) -> ClientBuilder {
        self.auto_connect = true;
        match fail_fast {
            FailFast::Yes => self.auto_connect_fail_fast = true,
            FailFast::No => self.auto_connect_fail_fast = false,
        }

        self
    }

    pub fn listen(mut self, url: Url) -> ClientBuilder {
        self.bind_url = Some(url);
        self
    }

    pub fn expire_session_after(mut self, expiration: Duration) -> Self {
        self.session_expiration = Some(expiration);
        self
    }

    pub fn tcp_max_recv_buffer_size(mut self, max: usize) -> anyhow::Result<Self> {
        self.stack_config.tcp_mem.rx.set_max(max)?;
        Ok(self)
    }

    pub fn tcp_max_send_buffer_size(mut self, max: usize) -> anyhow::Result<Self> {
        self.stack_config.tcp_mem.tx.set_max(max)?;
        Ok(self)
    }

    pub fn udp_max_recv_buffer_size(mut self, max: usize) -> anyhow::Result<Self> {
        self.stack_config.udp_mem.rx.set_max(max)?;
        Ok(self)
    }

    pub fn udp_max_send_buffer_size(mut self, max: usize) -> anyhow::Result<Self> {
        self.stack_config.udp_mem.tx.set_max(max)?;
        Ok(self)
    }

    pub async fn build_config(mut self) -> anyhow::Result<ClientConfig> {
        let bind_url = self
            .bind_url
            .unwrap_or_else(|| Url::parse("udp://0.0.0.0:0").unwrap());
        let crypto = self
            .crypto
            .unwrap_or_else(|| Rc::new(FallbackCryptoProvider::default()));

        let default_id = crypto.default_id().await?;
        let default_pub_key = crypto.get(default_id).await?.public_key().await?;

        self.stack_config.max_transmission_unit =
            resolve_max_payload_overhead_size(MAX_TAG_SIZE + Forward::header_size()).await?;

        Ok(ClientConfig {
            node_id: default_id,
            node_pub_key: default_pub_key,
            crypto,
            challenge_difficulty: 1,
            bind_url,
            srv_addr: parse_udp_url(&self.srv_url)?.parse()?,
            auto_connect: self.auto_connect,
            auto_connect_fail_fast: self.auto_connect_fail_fast,
            session_expiration: self
                .session_expiration
                .unwrap_or_else(|| Duration::from_secs(25)),
            stack_config: self.stack_config,
            ping_measure_interval: Duration::from_secs(300),
            session_request_timeout: Duration::from_millis(3000),
            challenge_request_timeout: Duration::from_millis(8000),
            reverse_connection_tmp_timeout: Duration::from_secs(3),
            reverse_connection_real_timeout: Duration::from_secs(13),
            incoming_session_timeout: Duration::from_secs(16),
            neighbourhood_ttl: Duration::from_secs(300),
            registry_config: Default::default(),
        })
    }

    pub async fn build(self) -> anyhow::Result<Client> {
        let mut client = Client::new(self.build_config().await?);

        client.spawn().await?;
        Ok(client)
    }
}

impl ClientConfig {
    pub async fn public_key(&self) -> Result<PublicKey, InternalError> {
        let crypto = self
            .crypto
            .get(self.node_id)
            .await
            .map_err(|e| InternalError::Generic(e.to_string()))?;
        crypto
            .public_key()
            .await
            .map_err(|e| InternalError::Generic(e.to_string()))
    }
}
