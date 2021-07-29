pub use ethsign::{KeyFile, Protected, SecretKey};
use ethsign::keyfile::Bytes;
use rand::Rng;
use std::io::Write;

const KEY_ITERATIONS: u32 = 2;
const KEYSTORE_VERSION: u64 = 3;

pub fn load_or_generate(path: &str, password: Option<Protected>) -> SecretKey {
    let password = password.unwrap_or(Protected::from("".to_string()));
    log::debug!("load_or_generate({}, {:?})", path, &password);
    if let Ok(file) = std::fs::File::open(path) {
        // Broken keyfile should panic
        let key: KeyFile = serde_json::from_reader(file).unwrap();
        // Invalid password should panic
        let secret = key.to_secret_key(&password).unwrap();
        log::info!("Loaded key. path={}", path);
        return secret;
    }
    // File does not exist, create new key
    let random_bytes: [u8; 32] = rand::thread_rng().gen();
    let secret = SecretKey::from_raw(random_bytes.as_ref()).unwrap();
    let key_file = KeyFile {
        id: format!("{}", uuid::Uuid::new_v4()),
        version: KEYSTORE_VERSION,
        crypto: secret.to_crypto(&password, KEY_ITERATIONS).unwrap(),
        address: Some(Bytes(secret.public().address().to_vec())),
    };
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(serde_json::to_string_pretty(&key_file).unwrap().as_ref())
        .unwrap();
    log::info!("Generated new key. path={}", path);
    secret
}
