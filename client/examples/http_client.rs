use structopt::StructOpt;

use ya_relay_core::key::{load_or_generate, Protected};

#[derive(StructOpt)]
struct Cli {
    #[structopt(long, env = "PORT")]
    port: u16,
    #[structopt(long, env = "RELAY_ADDR")]
    relay_addr: url::Url,
    #[structopt(long, env = "CLIENT_KEY_FILE")]
    key_file: String,
    #[structopt(long, env = "CLIENT_KEY_PASSWORD", parse(from_str = Protected::from))]
    key_password: Option<Protected>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::from_args();
}
