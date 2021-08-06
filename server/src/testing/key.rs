use ethsign::keyfile::Bytes;
pub use ethsign::{KeyFile, Protected, SecretKey};
use rand::Rng;
use std::fs::File;
use std::io::Write;

const KEY_ITERATIONS: u32 = 2;
const KEYSTORE_VERSION: u64 = 3;

pub fn generate() -> SecretKey {
    let random_bytes: [u8; 32] = rand::thread_rng().gen();
    SecretKey::from_raw(random_bytes.as_ref()).unwrap()
}

pub fn load_or_generate(path: &str, password: Option<Protected>) -> SecretKey {
    log::debug!("load_or_generate({}, {:?})", path, &password);

    // Default password = "", only use for testing purposes!
    let password = password.unwrap_or_else(|| Protected::from("".to_string()));

    if let Some(secret) = try_load_from_file(path, &password) {
        log::info!("Loaded key. path={}", path);
        return secret;
    }
    // File does not exist, create new key
    let secret = generate();
    save_to_file(path, &secret, &password);

    log::info!("Generated new key. path={}", path);
    secret
}

fn try_load_from_file(path: &str, password: &Protected) -> Option<SecretKey> {
    if let Ok(file) = File::open(path) {
        // Broken keyfile should panic
        let key: KeyFile = serde_json::from_reader(file).unwrap();
        // Invalid password should panic
        let secret = key.to_secret_key(password).unwrap();

        return Some(secret);
    }
    None
}

fn save_to_file(path: &str, secret: &SecretKey, password: &Protected) {
    let mut file = File::create(path).unwrap();
    let key_file = KeyFile {
        id: format!("{}", uuid::Uuid::new_v4()),
        version: KEYSTORE_VERSION,
        crypto: secret.to_crypto(password, KEY_ITERATIONS).unwrap(),
        address: Some(Bytes(secret.public().address().to_vec())),
    };
    let pretty_key_file_str = serde_json::to_string_pretty(&key_file).unwrap();
    file.write_all(pretty_key_file_str.as_ref()).unwrap();
}
