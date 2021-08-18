use digest::{Digest, Output};
use std::marker::PhantomData;

pub type DefaultChallenge<'k> = SignedChallenge<'k, LeadingZeros<sha3::Sha3_512>>;
pub const SIGNATURE_SIZE: usize = std::mem::size_of::<ethsign::Signature>();

pub trait Challenge {
    fn solve(&self, challenge: &[u8], difficulty: u64) -> anyhow::Result<Vec<u8>>;
    fn validate(&self, challenge: &[u8], difficulty: u64, response: &[u8]) -> anyhow::Result<bool>;
}

#[derive(Default)]
pub struct LeadingZeros<D: Digest> {
    marker: PhantomData<D>,
}

impl<D: Digest> Challenge for LeadingZeros<D> {
    fn solve(&self, challenge: &[u8], difficulty: u64) -> anyhow::Result<Vec<u8>> {
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
                anyhow::anyhow!("Could not find hash for difficulty {}", difficulty)
            })?;
        }
    }

    fn validate(&self, challenge: &[u8], difficulty: u64, response: &[u8]) -> anyhow::Result<bool> {
        if response.len() < 8 {
            anyhow::bail!("Invalid response size: {}", response.len());
        }

        let prefix = &response[0..8];
        let to_verify = &response[8..];
        let expected = digest::<D>(prefix, challenge);
        let zeros = leading_zeros(expected.as_slice());

        Ok(expected.as_slice() == to_verify && zeros >= difficulty)
    }
}

pub struct SignedChallenge<'k, C: Challenge> {
    challenge: C,
    key: Key<'k>,
}

impl<'k, C: Challenge> Challenge for SignedChallenge<'k, C> {
    fn solve(&self, challenge: &[u8], difficulty: u64) -> anyhow::Result<Vec<u8>> {
        let solved = self.challenge.solve(challenge, difficulty)?;
        let message = sha2::Sha256::digest(solved.as_slice());
        let sig = self.key.sign(message.as_slice())?;

        let mut result = Vec::with_capacity(SIGNATURE_SIZE + solved.len());
        result.push(sig.v);
        result.extend_from_slice(&sig.r[..]);
        result.extend_from_slice(&sig.s[..]);
        result.extend(solved.into_iter());
        Ok(result)
    }

    fn validate(&self, challenge: &[u8], difficulty: u64, response: &[u8]) -> anyhow::Result<bool> {
        if response.len() < SIGNATURE_SIZE {
            anyhow::bail!(
                "Invalid signature size: {}, expected {}",
                response.len(),
                SIGNATURE_SIZE
            );
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

        if self.key.public_eq(recovered_key.bytes()) {
            self.challenge.validate(challenge, difficulty, embedded)
        } else {
            Ok(false)
        }
    }
}

impl<'k, C: Challenge> SignedChallenge<'k, C> {
    pub fn new(challenge: C, key: impl Into<Key<'k>>) -> Self {
        let key = key.into();
        Self { challenge, key }
    }

    pub fn with(key: impl Into<Key<'k>>) -> Self
    where
        C: Default,
    {
        Self::new(C::default(), key)
    }
}

pub enum Key<'k> {
    PublicRaw(&'k [u8]),
    Secret(ethsign::SecretKey, ethsign::PublicKey),
}

impl<'k> From<&'k [u8]> for Key<'k> {
    fn from(key: &'k [u8]) -> Self {
        Self::PublicRaw(key)
    }
}

impl<'k> From<ethsign::SecretKey> for Key<'k> {
    fn from(secret: ethsign::SecretKey) -> Self {
        let public = secret.public();
        Self::Secret(secret, public)
    }
}

impl<'k> Key<'k> {
    pub fn sign(&self, message: &[u8]) -> anyhow::Result<ethsign::Signature> {
        match self {
            Key::Secret(secret, _) => Ok(secret.sign(message)?),
            _ => anyhow::bail!("Not a secret key"),
        }
    }

    pub fn public_eq(&self, key: &[u8]) -> bool {
        match self {
            Key::PublicRaw(bytes) => *bytes == key,
            Key::Secret(_, public) => &public.bytes()[..] == key,
        }
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
