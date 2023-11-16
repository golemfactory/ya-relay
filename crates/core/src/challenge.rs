use crate::crypto::Crypto;
use anyhow::{anyhow, bail};
use std::convert::{TryFrom};

use ethsign::PublicKey;
use futures::{Future, StreamExt, TryStreamExt};
use rand::Rng;
use digest::{Digest, Output, Update};
use sha2::Sha256;

use crate::identity::Identity;
use ya_client_model::NodeId;
use ya_relay_proto::proto;

pub const SIGNATURE_SIZE: usize = std::mem::size_of::<ethsign::Signature>();
pub const PREFIX_SIZE: usize = std::mem::size_of::<u64>();

pub const CHALLENGE_SIZE: usize = 16;
pub const CHALLENGE_DIFFICULTY: u64 = 16;

pub type RawChallenge = [u8; CHALLENGE_SIZE];

pub type ChallengeDigest = sha3::Sha3_512;

pub fn solve<'a, D: Digest, C: Crypto + 'a>(
    challenge: Vec<u8>,
    difficulty: u64,
    crypto_vec: Vec<C>,
) -> impl Future<Output = anyhow::Result<proto::ChallengeResponse>> + 'a {
    // Compute challenge in different thread to avoid blocking the runtime.
    // Note: computing starts here, not after awaiting.
    let challenge_handle =
        tokio::task::spawn_blocking(move || solve_challenge::<D>(challenge.as_slice(), difficulty));

    async move {
        let solution = challenge_handle.await??;
        let message = sha2::Sha256::digest(solution.as_slice());
        let signatures: anyhow::Result<Vec<_>> = futures::stream::iter(crypto_vec)
            .then(|crypto| sign(message.as_slice(), crypto))
            .try_collect()
            .await;

        Ok(proto::ChallengeResponse {
            solution,
            signatures: signatures?,
            session_sign: todo!(),
            session_pub_key: todo!()
        })
    }
}

fn solve_challenge<D: Digest>(challenge: &[u8], difficulty: u64) -> anyhow::Result<Vec<u8>> {
    let mut counter: u64 = 0;
    loop {
        let prefix = counter.to_be_bytes();
        let result = digest::<D>(&prefix, challenge);

        if leading_zeros(&result) >= difficulty {
            let mut response = prefix.to_vec();
            response.reserve(result.len());
            response.extend(result.into_iter());
            return Ok(response);
        }

        counter = counter.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("Could not find a hash for difficulty {}", difficulty)
        })?;
    }
}

pub fn verify<D: Digest>(
    challenge: &[u8],
    difficulty: u64,
    solution: &[u8],
) -> anyhow::Result<bool> {
    if solution.len() <= PREFIX_SIZE {
        anyhow::bail!("Invalid challenge solution size: {}", solution.len());
    }

    let prefix = &solution[0..PREFIX_SIZE];
    let to_verify = &solution[PREFIX_SIZE..];
    let expected = digest::<D>(prefix, challenge);
    let zeros = leading_zeros(expected.as_slice());

    Ok(expected.as_slice() == to_verify && zeros >= difficulty)
}


fn hash_session_key(solution : &[u8], session_pub_key : &[u8]) -> Output<Sha256> {
    fn _hash_session_key<D: Digest>(solution: &[u8], session_pub_key: &[u8]) -> Output<D> {
        D::new()
            .chain(b"golem session key")
            .chain(solution)
            .chain(session_pub_key)
            .finalize()
    }

    _hash_session_key::<Sha256>(solution, session_pub_key)
}

#[test]
fn test_hash_session_key() {

    let v = hash_session_key(&[], b"test");
    eprintln!("v={:?}",v);
}

pub fn recover_identities_from_challenge<D: Digest>(
    raw_challenge: &[u8],
    difficulty: u64,
    response: Option<proto::ChallengeResponse>,
    remote_id: Option<NodeId>,
) -> anyhow::Result<(NodeId, Vec<Identity>, Option<PublicKey>)> {
    let response = response.ok_or_else(|| anyhow::anyhow!("Missing ChallengeResponse"))?;

    if !verify::<D>(raw_challenge, difficulty, &response.solution)? {
        anyhow::bail!("Challenge verification failed");
    }

    let message = sha2::Sha256::digest(&response.solution);
    let default_ident = {
        let sig = response
            .signatures
            .get(0)
            .ok_or_else(|| anyhow::anyhow!("Missing signature"))?;

        let key = recover(sig.as_slice(), message.as_slice())?;
        Identity::from(key)
    };

    let default_id = default_ident.node_id;
    if let Some(remote_id) = remote_id {
        if default_id != remote_id {
            anyhow::bail!("Invalid default NodeId [{remote_id}] vs [{default_id}] (response)");
        }
    }

    let identities = std::iter::once(Ok(default_ident))
        .chain(response.signatures.iter().skip(1).map(|sig| {
            let key = recover(sig.as_slice(), message.as_slice())?;
            Ok(Identity::from(key))
        }))
        .collect::<anyhow::Result<_>>()?;

    if !response.session_sign.is_empty()  {
        let session_pub_key = PublicKey::from_slice(&response.session_pub_key)
            .map_err(|_| anyhow!("Failed to decode session key"))?;

        for (signature, identity) in response.session_sign.iter().zip(&identities) {
            let identity : &Identity = identity;
            let m  = hash_session_key(&response.solution, &response.session_pub_key);
            let id_key = recover(signature, m.as_slice())?;
            if id_key.bytes() != identity.public_key.bytes() {
                bail!("invalid session key identity signature");
            }
        }
        Ok((default_id, identities, Some(session_pub_key)))
    }
    else {
        Ok((default_id, identities, None))
    }
}

pub fn recover_default_node_id(request: &proto::request::Session) -> anyhow::Result<NodeId> {
    match request.identities.get(0) {
        Some(identity) => Ok(NodeId::try_from(&identity.node_id)?),
        None => bail!("First session request has empty identities vector."),
    }
}

async fn sign(message: &[u8], crypto: impl Crypto) -> anyhow::Result<Vec<u8>> {
    let sig = crypto.sign(message).await?;

    let mut result = Vec::with_capacity(SIGNATURE_SIZE);
    result.push(sig.v);
    result.extend_from_slice(&sig.r[..]);
    result.extend_from_slice(&sig.s[..]);

    Ok(result)
}

fn recover(sig: &[u8], message: &[u8]) -> anyhow::Result<PublicKey> {
    let len = sig.len();
    if len != SIGNATURE_SIZE {
        anyhow::bail!(
            "Invalid signature size: {} vs {} B (response)",
            SIGNATURE_SIZE,
            len,
        );
    }

    let v = sig[0];
    let mut r = [0; 32];
    let mut s = [0; 32];

    r.copy_from_slice(&sig[1..33]);
    s.copy_from_slice(&sig[33..]);

    Ok(ethsign::Signature { v, r, s }.recover(message)?)
}

fn digest<D: Digest>(nonce: &[u8], input: &[u8]) -> Output<D> {
    let mut hasher = D::new();
    hasher.update(nonce);
    hasher.update(input);
    hasher.finalize()
}

fn leading_zeros(result: &[u8]) -> u64 {
    let mut total: u64 = 0;
    for byte in result.iter() {
        if *byte == 0 {
            total += 8;
        } else {
            total += (*byte).leading_zeros() as u64;
            break;
        }
    }
    total
}

pub fn prepare_challenge(difficulty: u64) -> (proto::ChallengeRequest, RawChallenge) {
    let raw_challenge = rand::thread_rng().gen::<RawChallenge>();
    let request = proto::ChallengeRequest {
        version: "0.0.1".to_string(),
        caps: 0,
        kind: proto::challenge_request::Kind::Sha3512LeadingZeros as i32,
        difficulty,
        challenge: raw_challenge.to_vec(),
    };
    (request, raw_challenge)
}

pub fn prepare_challenge_request(difficulty: u64) -> (proto::request::Session, RawChallenge) {
    let (challenge, raw_challenge) = prepare_challenge(difficulty);
    let request = proto::request::Session {
        challenge_req: Some(challenge),
        ..Default::default()
    };
    (request, raw_challenge)
}

pub fn prepare_challenge_response(difficulty: u64) -> (proto::response::Session, RawChallenge) {
    let (challenge, raw_challenge) = prepare_challenge(difficulty);
    let response = proto::response::Session {
        challenge_req: Some(challenge),
        ..Default::default()
    };
    (response, raw_challenge)
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use anyhow;
    use ethsign::PublicKey;
    use futures::{StreamExt, TryStreamExt};
    use rand::Rng;

    use crate::challenge::ChallengeDigest;
    use crate::crypto::{Crypto, CryptoProvider, FallbackCryptoProvider};
    use ya_client_model::NodeId;

    async fn gen_crypto(n: usize) -> anyhow::Result<(Vec<PublicKey>, Vec<Rc<dyn Crypto>>)> {
        let pairs: Vec<_> = futures::stream::iter((0..n).map(anyhow::Ok))
            .then(|_| async {
                let bytes = rand::thread_rng().gen::<[u8; 32]>();
                let secret = ethsign::SecretKey::from_raw(&bytes)?;
                let public = secret.public();

                let provider = FallbackCryptoProvider::new(secret);
                let crypto = provider.get(provider.default_id().await?).await?;

                Ok::<_, anyhow::Error>((public, crypto))
            })
            .try_collect()
            .await?;

        Ok(pairs.into_iter().unzip())
    }

    #[tokio::test]
    async fn sign_verify_recover() -> anyhow::Result<()> {
        const DIFFICULTY: u64 = 2;

        let (keys, crypto_vec) = gen_crypto(3).await?;
        let challenge: Vec<u8> = (0..16).collect();

        let response =
            super::solve::<ChallengeDigest, _>(challenge.clone(), DIFFICULTY, crypto_vec).await?;

        let (node_id, identities, _) = super::recover_identities_from_challenge::<ChallengeDigest>(
            challenge.as_slice(),
            DIFFICULTY,
            Some(response),
            None,
        )?;

        assert_eq!(identities.len(), keys.len());
        assert_eq!(identities[0].node_id, node_id);

        for i in 0..keys.len() {
            assert_eq!(
                identities[i].node_id,
                NodeId::from(keys[i].address().as_slice())
            );
        }

        Ok(())
    }
}
