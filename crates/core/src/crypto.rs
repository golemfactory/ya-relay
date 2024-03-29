use std::collections::HashMap;
use std::rc::Rc;

pub use ethsign::{PublicKey, SecretKey, Signature};
use futures::future::LocalBoxFuture;
use futures::FutureExt;

use ya_client_model::NodeId;

use crate::key::generate;

pub trait CryptoProvider {
    fn default_id<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<NodeId>>;
    fn aliases<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<Vec<NodeId>>>;
    fn get<'a>(&self, node_id: NodeId) -> LocalBoxFuture<'a, anyhow::Result<Rc<dyn Crypto>>>;
}

pub trait Crypto {
    fn public_key<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<PublicKey>>;
    fn sign<'a>(&self, message: &'a [u8]) -> LocalBoxFuture<'a, anyhow::Result<Signature>>;
    fn encrypt<'a>(
        &self,
        message: &'a [u8],
        remote_key: &'a PublicKey,
    ) -> LocalBoxFuture<'a, anyhow::Result<Vec<u8>>>;
}

impl<C: CryptoProvider + ?Sized> CryptoProvider for Rc<C> {
    fn default_id<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<NodeId>> {
        (**self).default_id()
    }

    fn aliases<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<Vec<NodeId>>> {
        (**self).aliases()
    }

    fn get<'a>(&self, node_id: NodeId) -> LocalBoxFuture<'a, anyhow::Result<Rc<dyn Crypto>>> {
        (**self).get(node_id)
    }
}

impl<C: Crypto + ?Sized> Crypto for Rc<C> {
    fn public_key<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<PublicKey>> {
        (**self).public_key()
    }

    fn sign<'a>(&self, message: &'a [u8]) -> LocalBoxFuture<'a, anyhow::Result<Signature>> {
        (**self).sign(message)
    }

    fn encrypt<'a>(
        &self,
        message: &'a [u8],
        remote_key: &'a PublicKey,
    ) -> LocalBoxFuture<'a, anyhow::Result<Vec<u8>>> {
        (**self).encrypt(message, remote_key)
    }
}

#[derive(Clone)]
pub struct FallbackCryptoProvider {
    default_id: NodeId,
    inner: HashMap<NodeId, FallbackCrypto>,
}

impl FallbackCryptoProvider {
    pub fn new(secret: SecretKey) -> Self {
        let crypto: FallbackCrypto = secret.into();
        Self {
            default_id: crypto.id,
            inner: vec![(crypto.id, crypto)].into_iter().collect(),
        }
    }

    pub fn add(&mut self, secret: SecretKey) {
        let crypto: FallbackCrypto = secret.into();
        self.inner.insert(crypto.id, crypto);
    }

    pub fn default_node_id(&self) -> NodeId {
        self.default_id
    }
}

impl Default for FallbackCryptoProvider {
    fn default() -> Self {
        Self::new(generate())
    }
}

impl CryptoProvider for FallbackCryptoProvider {
    fn default_id<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<NodeId>> {
        futures::future::ok(self.default_id).boxed_local()
    }

    fn aliases<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<Vec<NodeId>>> {
        let aliases = self
            .inner
            .keys()
            .filter(|id| *id != &self.default_id)
            .copied()
            .collect();
        futures::future::ok(aliases).boxed_local()
    }

    fn get<'a>(&self, node_id: NodeId) -> LocalBoxFuture<'a, anyhow::Result<Rc<dyn Crypto>>> {
        let result = self
            .inner
            .get(&node_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown node id: {}", node_id))
            .map(Rc::new);
        Box::pin(async move { Ok::<Rc<dyn Crypto>, _>(result?) }.boxed_local())
    }
}

#[derive(Clone)]
pub struct FallbackCrypto {
    id: NodeId,
    secret: SecretKey,
}

impl From<SecretKey> for FallbackCrypto {
    fn from(secret: SecretKey) -> Self {
        let id = NodeId::from(*secret.public().address());
        Self { id, secret }
    }
}

impl Crypto for FallbackCrypto {
    fn public_key<'a>(&self) -> LocalBoxFuture<'a, anyhow::Result<PublicKey>> {
        let public = self.secret.public();
        async move { Ok(public) }.boxed_local()
    }

    fn sign<'a>(&self, message: &'a [u8]) -> LocalBoxFuture<'a, anyhow::Result<Signature>> {
        let result = self.secret.sign(message);
        async move { Ok(result?) }.boxed_local()
    }

    fn encrypt<'a>(
        &self,
        _message: &'a [u8],
        _remote_key: &'a PublicKey,
    ) -> LocalBoxFuture<'a, anyhow::Result<Vec<u8>>> {
        unimplemented!()
    }
}
