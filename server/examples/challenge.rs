use ya_net_server::challenge::{solve_challenge, validate_challenge};

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let sample: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let difficulty = 2;

    log::info!("Computing challenge");

    let result = solve_challenge(&sample, difficulty);

    log::info!("Seed: {:?}", &result[0..4]);
    log::info!("Result: {:?}", &result[4..68]);
    log::info!("length: {:?}", result.len());

    let check = validate_challenge(&sample, &result, difficulty);
    log::info!("Check: {}", check);

    Ok(())
}
