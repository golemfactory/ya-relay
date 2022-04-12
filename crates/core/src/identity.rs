use std::convert::{TryFrom, TryInto};
use std::hash::{Hash, Hasher};

use anyhow::{anyhow, bail};
use ethsign::PublicKey;

use ya_client_model::NodeId;
use ya_relay_proto::proto;

#[derive(Clone)]
pub struct Identity {
    pub node_id: NodeId,
    pub public_key: PublicKey,
}

impl Hash for Identity {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.node_id.hash(state)
    }
}

impl PartialEq for Identity {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
    }
}

impl Eq for Identity {}

impl From<(NodeId, PublicKey)> for Identity {
    fn from(tuple: (NodeId, PublicKey)) -> Self {
        Self {
            node_id: tuple.0,
            public_key: tuple.1,
        }
    }
}

impl From<Identity> for proto::Identity {
    fn from(ident: Identity) -> Self {
        proto::Identity {
            public_key: ident.public_key.bytes().to_vec(),
            node_id: ident.node_id.into_array().to_vec(),
        }
    }
}

impl<'a> From<&'a Identity> for proto::Identity {
    fn from(ident: &'a Identity) -> Self {
        proto::Identity {
            public_key: ident.public_key.bytes().to_vec(),
            node_id: ident.node_id.into_array().to_vec(),
        }
    }
}

impl From<PublicKey> for Identity {
    fn from(public_key: PublicKey) -> Self {
        let node_id = NodeId::from(public_key.address().as_ref());
        Self {
            public_key,
            node_id,
        }
    }
}

impl<'a> TryFrom<&'a [u8]> for Identity {
    type Error = anyhow::Error;

    fn try_from(slice: &'a [u8]) -> Result<Self, Self::Error> {
        let public_key = PublicKey::from_slice(slice).map_err(|_| anyhow!("Invalid public key"))?;
        let node_id = NodeId::from(public_key.address().as_ref());

        Ok(Self {
            node_id,
            public_key,
        })
    }
}

impl<'a> TryFrom<&'a proto::Identity> for Identity {
    type Error = anyhow::Error;

    fn try_from(ident: &'a proto::Identity) -> Result<Self, Self::Error> {
        let public_key =
            PublicKey::from_slice(&ident.public_key).map_err(|_| anyhow!("Invalid public key"))?;
        let node_id = NodeId::from(public_key.address().as_ref());

        if node_id.as_ref() != ident.node_id {
            bail!("Mismatched NodeId");
        }

        Ok(Self {
            node_id,
            public_key,
        })
    }
}

impl<'a> TryFrom<&'a proto::response::Node> for Identity {
    type Error = anyhow::Error;

    fn try_from(node: &'a proto::response::Node) -> Result<Self, Self::Error> {
        node.identities
            .get(0)
            .ok_or_else(|| anyhow!("Missing Node identity"))?
            .try_into()
    }
}
