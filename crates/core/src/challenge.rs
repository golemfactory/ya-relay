use crate::crypto::Crypto;

use digest::{Digest, Output};
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use rand::Rng;

use ya_relay_proto::proto;

pub const SIGNATURE_SIZE: usize = std::mem::size_of::<ethsign::Signature>();
pub const PREFIX_SIZE: usize = std::mem::size_of::<u64>();

pub const CHALLENGE_SIZE: usize = 16;
pub const CHALLENGE_DIFFICULTY: u64 = 16;

pub async fn solve(
    challenge: &[u8],
    difficulty: u64,
    crypto: impl Crypto,
) -> anyhow::Result<Vec<u8>> {
    let solution = solve_challenge::<sha3::Sha3_512>(challenge, difficulty)?;
    sign(solution, crypto).await
}

pub fn solve_blocking<'a>(
    challenge: Vec<u8>,
    difficulty: u64,
    crypto: impl Crypto + 'a,
) -> LocalBoxFuture<'a, anyhow::Result<Vec<u8>>> {
    // Compute challenge in different thread to avoid blocking runtime.
    // Note: computing starts here, not after awaiting.
    let challenge_handle = tokio::task::spawn_blocking(move || {
        solve_challenge::<sha3::Sha3_512>(challenge.as_slice(), difficulty)
    });

    async move {
        let solution = challenge_handle.await??;
        sign(solution, crypto).await
    }
    .boxed_local()
}

pub fn verify(
    challenge: &[u8],
    difficulty: u64,
    response: &[u8],
    pub_key: &[u8],
) -> anyhow::Result<bool> {
    let inner = verify_signature(response, pub_key)?;
    verify_challenge::<sha3::Sha3_512>(challenge, difficulty, inner)
}

pub fn solve_challenge<D: Digest>(challenge: &[u8], difficulty: u64) -> anyhow::Result<Vec<u8>> {
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

        counter = counter
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Could not find hash for difficulty {}", difficulty))?;
    }
}

pub fn verify_challenge<D: Digest>(
    challenge: &[u8],
    difficulty: u64,
    response: &[u8],
) -> anyhow::Result<bool> {
    if response.len() < PREFIX_SIZE {
        anyhow::bail!("Invalid response size: {}", response.len());
    }

    let prefix = &response[0..PREFIX_SIZE];
    let to_verify = &response[PREFIX_SIZE..];
    let expected = digest::<D>(prefix, challenge);
    let zeros = leading_zeros(expected.as_slice());

    Ok(expected.as_slice() == to_verify && zeros >= difficulty)
}

pub async fn sign(solution: Vec<u8>, crypto: impl Crypto) -> anyhow::Result<Vec<u8>> {
    let message = sha2::Sha256::digest(solution.as_slice());
    let sig = crypto.sign(message.as_slice()).await?;

    let mut result = Vec::with_capacity(SIGNATURE_SIZE + solution.len());
    result.push(sig.v);
    result.extend_from_slice(&sig.r[..]);
    result.extend_from_slice(&sig.s[..]);
    result.extend(solution.into_iter());

    Ok(result)
}

pub fn verify_signature<'b>(response: &'b [u8], pub_key: &[u8]) -> anyhow::Result<&'b [u8]> {
    let len = response.len();
    if len < SIGNATURE_SIZE {
        anyhow::bail!("Signature too short: {} out of {} B", len, SIGNATURE_SIZE);
    }

    let sig = &response[..SIGNATURE_SIZE];
    let embedded = &response[SIGNATURE_SIZE..];

    let v = sig[0];
    let mut r = [0; 32];
    let mut s = [0; 32];

    r.copy_from_slice(&sig[1..33]);
    s.copy_from_slice(&sig[33..]);

    let message = sha2::Sha256::digest(embedded);
    let recovered_key = ethsign::Signature { v, r, s }.recover(message.as_slice())?;

    if pub_key == recovered_key.bytes() {
        Ok(embedded)
    } else {
        anyhow::bail!("Invalid public key");
    }
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

pub fn prepare_challenge() -> (proto::ChallengeRequest, [u8; CHALLENGE_SIZE]) {
    let raw_challenge = rand::thread_rng().gen::<[u8; CHALLENGE_SIZE]>();
    let request = proto::ChallengeRequest {
        version: "0.0.1".to_string(),
        caps: 0,
        kind: 10,
        difficulty: CHALLENGE_DIFFICULTY as u64,
        challenge: raw_challenge.to_vec(),
    };
    (request, raw_challenge)
}

pub fn prepare_challenge_request() -> (proto::request::Session, [u8; CHALLENGE_SIZE]) {
    let (challenge, raw_challenge) = prepare_challenge();
    let request = proto::request::Session {
        node_id: vec![],
        public_key: vec![],
        challenge_req: Some(challenge),
        challenge_resp: vec![],
        supported_encryptions: vec![],
    };
    (request, raw_challenge)
}

pub fn prepare_challenge_response() -> (proto::response::Session, [u8; CHALLENGE_SIZE]) {
    let (challenge, raw_challenge) = prepare_challenge();
    let response = proto::response::Session {
        public_key: vec![],
        challenge_req: Some(challenge),
        challenge_resp: vec![],
        chosen_encryption: vec![],
    };
    (response, raw_challenge)
}
