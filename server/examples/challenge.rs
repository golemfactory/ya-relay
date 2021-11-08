use rand::Rng;
use ya_relay_server::challenge::{self, PREFIX_SIZE, SIGNATURE_SIZE};
use ya_relay_server::crypto::{CryptoProvider, FallbackCryptoProvider};

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let bytes = rand::thread_rng().gen::<[u8; 32]>();
    let secret = ethsign::SecretKey::from_raw(&bytes)?;
    let public = secret.public();
    let public_bytes = public.bytes().to_vec();

    let sample: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let difficulty = 16;

    log::info!("Computing challenge");

    let provider = FallbackCryptoProvider::new(secret);
    let crypto = provider.get(provider.default_id().await?).await?;
    let response = challenge::solve(&sample, difficulty, crypto).await?;
    let result = &response[SIGNATURE_SIZE..];

    log::info!("Prefix: {:?}", &result[0..PREFIX_SIZE]);
    log::info!("Result: {:?}", &result[PREFIX_SIZE..]);
    log::info!("Length: {:?}", result.len());

    let check = challenge::verify(
        &sample,
        difficulty,
        response.as_slice(),
        public_bytes.as_slice(),
    )?;
    log::info!("Check: {}", check);

    Ok(())
}
