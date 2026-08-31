use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use ethsign::PublicKey;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

use crate::challenge::verify_session_key;
use crate::identity::Identity;
use ya_client_model::NodeId;
use ya_relay_proto::proto;
use ya_relay_proto::proto::{SlotId, SESSION_ID_SIZE};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, derive_more::Display, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportType {
    Unreliable,
    Reliable,
    Transfer,
}

#[derive(Copy, Clone, PartialEq, PartialOrd, Hash, Eq, Ord, Serialize, Deserialize)]
pub struct SessionId {
    id: [u8; SESSION_ID_SIZE],
}

impl SessionId {
    pub fn to_array(&self) -> [u8; SESSION_ID_SIZE] {
        self.id
    }
}

impl AsRef<[u8]> for SessionId {
    fn as_ref(&self) -> &[u8] {
        self.id.as_ref()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub protocol: proto::Protocol,
    pub address: SocketAddr,
}

#[derive(Clone)]
pub struct NodeInfo {
    pub identities: Vec<Identity>,
    /// Identities which independently proved ownership of `session_key`.
    pub authenticated_identities: Vec<NodeId>,
    pub slot: SlotId,

    /// Endpoints registered by Node.
    pub endpoints: Vec<Endpoint>,
    pub supported_encryption: Vec<String>,
    pub session_key: Option<PublicKey>,
}

impl NodeInfo {
    pub fn default_node_id(&self) -> NodeId {
        self.identities.first().map(|ident| ident.node_id).unwrap()
    }

    pub fn public_key(&self) -> Vec<u8> {
        self.identities
            .first()
            .map(|ident| ident.public_key.bytes().to_vec())
            .unwrap()
    }
}

#[derive(Clone)]
pub struct LastSeen {
    last_seen: Arc<Mutex<DateTime<Utc>>>,
}

#[derive(Clone)]
pub struct RequestHistory {
    capacity: usize,
    ids: Arc<RwLock<VecDeque<u64>>>,
}

impl From<DateTime<Utc>> for LastSeen {
    fn from(datetime: DateTime<Utc>) -> Self {
        LastSeen {
            last_seen: Arc::new(Mutex::new(datetime)),
        }
    }
}

impl LastSeen {
    pub fn now() -> LastSeen {
        LastSeen::from(Utc::now())
    }

    pub fn update(&self, datetime: DateTime<Utc>) {
        *self.last_seen.lock().unwrap() = datetime
    }

    pub fn time(&self) -> DateTime<Utc> {
        *self.last_seen.lock().unwrap()
    }
}

impl RequestHistory {
    pub fn new(capacity: usize) -> Self {
        RequestHistory {
            capacity,
            ids: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    pub fn push(&self, request_id: u64) {
        let mut ids = self.ids.write().unwrap();

        if ids.len() == self.capacity {
            ids.pop_front();
        }

        ids.push_back(request_id);
    }

    pub fn contains(&self, request_id: u64) -> bool {
        self.ids.read().unwrap().contains(&request_id)
    }
}

impl TryFrom<Vec<u8>> for SessionId {
    type Error = anyhow::Error;

    fn try_from(session: Vec<u8>) -> Result<Self> {
        if session.len() != SESSION_ID_SIZE {
            bail!("Invalid SessionID: {}", String::from_utf8(session)?)
        }

        let mut id: [u8; SESSION_ID_SIZE] = [0; SESSION_ID_SIZE];
        session[0..SESSION_ID_SIZE]
            .iter()
            .enumerate()
            .for_each(|(i, s)| id[i] = *s);

        Ok(SessionId { id })
    }
}

impl TryFrom<&[u8]> for SessionId {
    type Error = anyhow::Error;

    fn try_from(session: &[u8]) -> Result<Self> {
        let id: [u8; SESSION_ID_SIZE] = session.try_into()?;

        Ok(SessionId { id })
    }
}

impl TryFrom<&str> for SessionId {
    type Error = anyhow::Error;

    fn try_from(session: &str) -> Result<Self> {
        SessionId::try_from(hex::decode(session)?)
    }
}

impl From<[u8; SESSION_ID_SIZE]> for SessionId {
    fn from(array: [u8; SESSION_ID_SIZE]) -> Self {
        SessionId { id: array }
    }
}

impl From<SessionId> for [u8; SESSION_ID_SIZE] {
    fn from(session: SessionId) -> [u8; SESSION_ID_SIZE] {
        session.id
    }
}

impl SessionId {
    pub fn generate() -> SessionId {
        SessionId {
            id: rand::thread_rng().gen::<[u8; SESSION_ID_SIZE]>(),
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.id.to_vec()
    }
}

impl<'a> PartialEq<&'a [u8]> for SessionId {
    fn eq(&self, other: &&'a [u8]) -> bool {
        &self.id[..] == *other
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", hex::encode(self.id))
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.id))
    }
}

impl TryFrom<proto::Endpoint> for Endpoint {
    type Error = anyhow::Error;

    fn try_from(endpoint: proto::Endpoint) -> Result<Self> {
        let address = format!("{}:{}", endpoint.address, endpoint.port);
        let address: SocketAddr = address.parse()?;

        Ok(Endpoint {
            protocol: endpoint
                .protocol
                .try_into()
                .map_err(|_| anyhow!("Invalid protocol enum: {}", endpoint.protocol))?,
            address,
        })
    }
}

impl From<Endpoint> for proto::Endpoint {
    fn from(endpoint: Endpoint) -> Self {
        proto::Endpoint {
            protocol: endpoint.protocol as i32,
            address: endpoint.address.ip().to_string(),
            port: endpoint.address.port() as u32,
        }
    }
}

impl TryFrom<proto::response::Node> for NodeInfo {
    type Error = anyhow::Error;

    fn try_from(value: proto::response::Node) -> std::result::Result<Self, Self::Error> {
        let identities = value
            .identities
            .iter()
            .map(Identity::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        let default_identity = identities
            .first()
            .ok_or_else(|| anyhow!("NodeInfo has no identities"))?;
        let mut authenticated_identities = Vec::new();

        let session_key = if !value.session_pub_key.is_empty() {
            if !value.session_key_proofs.is_empty()
                && value.session_key_proofs.len() != identities.len()
            {
                bail!(
                    "Invalid session key proof count: {} vs {} identities",
                    value.session_key_proofs.len(),
                    identities.len()
                );
            }

            if !value.session_key_proof.is_empty() {
                verify_session_key(
                    &value.session_pub_key,
                    &value.session_key_proof,
                    default_identity,
                )?;
                authenticated_identities.push(default_identity.node_id);
            }

            for (identity, proof) in identities.iter().zip(&value.session_key_proofs) {
                if proof.is_empty() {
                    continue;
                }
                verify_session_key(&value.session_pub_key, proof, identity)?;
                if !authenticated_identities.contains(&identity.node_id) {
                    authenticated_identities.push(identity.node_id);
                }
            }

            if !authenticated_identities.contains(&default_identity.node_id) {
                bail!("Missing default identity session key proof");
            }

            Some(
                PublicKey::from_slice(&value.session_pub_key)
                    .map_err(|_| anyhow!("Failed to decode session key"))?,
            )
        } else {
            None
        };

        Ok(NodeInfo {
            identities,
            authenticated_identities,
            slot: value.slot,
            endpoints: value
                .endpoints
                .into_iter()
                .map(Endpoint::try_from)
                .collect::<anyhow::Result<Vec<_>>>()?,
            supported_encryption: value.supported_encryptions,
            session_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::challenge::{self, ChallengeDigest};
    use crate::crypto::{FallbackCrypto, SessionCrypto};
    use crate::key::generate;

    async fn signed_node() -> (proto::response::Node, Vec<NodeId>) {
        let secrets = vec![generate(), generate()];
        let identities: Vec<Identity> = secrets
            .iter()
            .map(|secret| Identity::from(secret.public()))
            .collect();
        let node_ids = identities.iter().map(|identity| identity.node_id).collect();
        let session = SessionCrypto::generate().unwrap();
        let response = challenge::solve::<ChallengeDigest, _>(
            vec![0; 16],
            0,
            secrets.into_iter().map(FallbackCrypto::from).collect(),
            session.pub_key(),
        )
        .await
        .unwrap();

        (
            proto::response::Node {
                identities: identities.iter().map(Into::into).collect(),
                session_pub_key: response.session_pub_key,
                session_key_proof: response.session_sign[0].clone(),
                session_key_proofs: response.session_sign,
                ..Default::default()
            },
            node_ids,
        )
    }

    #[tokio::test]
    async fn authenticates_all_session_key_proofs() {
        let (node, node_ids) = signed_node().await;

        let info = NodeInfo::try_from(node).unwrap();

        assert_eq!(info.authenticated_identities, node_ids);
        assert!(info.session_key.is_some());
    }

    #[tokio::test]
    async fn accepts_legacy_default_session_key_proof() {
        let (mut node, node_ids) = signed_node().await;
        node.session_key_proofs.clear();

        let info = NodeInfo::try_from(node).unwrap();

        assert_eq!(info.authenticated_identities, vec![node_ids[0]]);
        assert!(info.session_key.is_some());
    }

    #[tokio::test]
    async fn rejects_partial_alias_session_key_proofs() {
        let (mut node, _) = signed_node().await;
        node.session_key_proofs.pop();

        assert!(NodeInfo::try_from(node).is_err());
    }

    #[tokio::test]
    async fn rejects_invalid_alias_session_key_proof() {
        let (mut node, _) = signed_node().await;
        node.session_key_proofs[1] = node.session_key_proofs[0].clone();

        assert!(NodeInfo::try_from(node).is_err());
    }
}
