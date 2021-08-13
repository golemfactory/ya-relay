use std::convert::TryInto;

use crate::server::CHALLENGE_SIZE;

use sha3::{Digest, Sha3_512};

// Currenly only used in examples and testing, not sure why its found to be dead.
#[allow(dead_code)]
pub fn solve_challenge(input: &[u8; CHALLENGE_SIZE], difficulty: usize) -> [u8; 68] {
    let mut seed: u32 = 0;
    loop {
        let seed_bytes = seed.to_be_bytes();
        let result = compute_challenge(input, &seed_bytes);
        if is_difficult(&result, difficulty) {
            let mut challenge_resp: Vec<u8> = seed_bytes.to_vec();
            challenge_resp.append(&mut result.to_vec());
            return challenge_resp.try_into().unwrap();
        }
        seed += 1;
        log::debug!("Checking new seed: {}", seed);
        if seed == u32::MAX {
            panic!("Could not find seed with difficulty");
        }
    }
}

pub fn validate_challenge(input: &[u8], raw_result: &[u8], difficulty: usize) -> bool {
    let seed = &raw_result[0..4];
    let result = &raw_result[4..68];
    if !is_difficult(result, difficulty) {
        return false;
    }
    let check = compute_challenge(input, seed);
    check == result
}

fn compute_challenge(input: &[u8], seed: &[u8]) -> Vec<u8> {
    // create a SHA3-256 object
    let mut hasher = Sha3_512::new();

    // write input message
    hasher.input(seed);
    hasher.input(input);

    // read hash digest
    hasher.result().to_vec()
}

fn is_difficult(result: &[u8], difficulty: usize) -> bool {
    if result.len() != 64 {
        return false;
    }
    for i in 0..difficulty {
        if result[i as usize] != 0u8 {
            return false;
        }
    }
    true
}
