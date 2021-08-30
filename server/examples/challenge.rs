use rand::Rng;
use ya_net_server::challenge::{Challenge, DefaultChallenge, SIGNATURE_SIZE};

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

    let challenge = DefaultChallenge::with(secret);
    let response = challenge.solve(&sample, difficulty)?;
    let result = &response[SIGNATURE_SIZE..];

    log::info!("Prefix: {:?}", &result[0..8]);
    log::info!("Result: {:?}", &result[8..72]);
    log::info!("Length: {:?}", result.len());

    let challenge = DefaultChallenge::with(public_bytes.as_slice());
    let check = challenge.validate(&sample, difficulty, response.as_slice())?;
    log::info!("Check: {}", check);

    Ok(())
}
