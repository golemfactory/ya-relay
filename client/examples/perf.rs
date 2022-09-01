use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::future::join_all;
use structopt::{clap, StructOpt};

use ya_relay_client::{Client, ClientBuilder};
use ya_relay_core::crypto::FallbackCryptoProvider;
use ya_relay_core::key::{load_or_generate, Protected};

#[derive(StructOpt)]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
pub struct Options {
    #[structopt(short = "a", env = "NET_ADDRESS")]
    pub address: url::Url,
    #[structopt(short = "f", long, env = "CLIENT_KEY_FILE")]
    key_file: Option<String>,
    #[structopt(short = "p", long, env = "CLIENT_KEY_PASSWORD", parse(from_str = Protected::from))]
    key_password: Option<Protected>,
    /// Where to save the resulting CSV
    #[structopt(long, help = "Defaults to stdout")]
    csv: Option<PathBuf>,
    #[structopt(subcommand)]
    command: Command,
}

/// Test type
#[derive(StructOpt)]
enum Command {
    /// Tests the maximum number of connections that can be sustained at the same time
    Connections(ConnectionsCommand),
    /// Measures responses / second
    Load(LoadCommand),
}

/// Connection test configuration
#[derive(StructOpt)]
pub struct ConnectionsCommand {
    /// How many connections should be established in one step
    #[structopt(short = "c", long)]
    connections_increment: usize,
}

/// Load test configuration
#[derive(StructOpt)]
pub struct LoadCommand {
    /// Number of connections to use
    #[structopt(long, use_delimiter = true)]
    connections: Vec<usize>,
    /// Artificial limit of requests per second
    #[structopt(long, use_delimiter = true)]
    rate_limits: Vec<u32>,
    /// Proportion of find_node calls
    #[structopt(long, default_value = "0")]
    find_node_weight: u32,
    /// Proportion of neighbours calls
    #[structopt(long, default_value = "0")]
    neighbours_weight: u32,
    /// Size of the neighbourhood to request
    #[structopt(long, default_value = "10")]
    neighbours_size: u32,
    /// Proportion of ping calls
    #[structopt(long, default_value = "0")]
    ping_weight: u32,
    /// Sample size in requests per connection
    #[structopt(long, default_value = "100")]
    requests_per_connection: NonZeroU32,
}

mod util {
    use super::*;

    /// Establishes one connection
    pub async fn establish_connection(args: &Options) -> anyhow::Result<Client> {
        let address = args.address.clone();
        let builder = if let Some(key_file) = &args.key_file {
            let password = args.key_password.clone();
            let secret = load_or_generate(key_file, password);
            ClientBuilder::from_url(address).crypto(FallbackCryptoProvider::new(secret))
        } else {
            ClientBuilder::from_url(address)
        };

        let client = builder.build().await?;
        client.neighbours(0).await?;

        Ok(client)
    }

    /// Establishes {connections} connections sequentially, appending to the {conns} buffer.
    pub async fn establish_connections(
        conns: &mut Vec<Client>,
        args: &Options,
        connections: usize,
    ) -> anyhow::Result<()> {
        for _ in 0..connections {
            let client = establish_connection(args).await?;
            conns.push(client);
        }

        Ok(())
    }
}

mod connections_test {
    use super::*;

    #[derive(Debug)]
    struct DataPoint {
        connections: u32,
        new_connections_rate: f32,
    }

    #[derive(Debug)]
    struct Measurements {
        data: Vec<DataPoint>,
    }

    impl Measurements {
        fn new() -> Self {
            Measurements { data: Vec::new() }
        }

        fn push(&mut self, connections: u32, new_connections_rate: f32) {
            self.data.push(DataPoint {
                connections,
                new_connections_rate,
            });
        }

        fn save(&self, writer: impl std::io::Write) -> anyhow::Result<()> {
            let mut w = csv::Writer::from_writer(writer);

            w.write_record(&["connections", "new-connections-rate"])?;
            for data_point in &self.data {
                w.write_record(&[
                    &data_point.connections.to_string(),
                    &data_point.new_connections_rate.to_string(),
                ])?;
            }

            Ok(())
        }
    }

    /// Establish connections in batches while asserting that none have been dropped
    async fn measure_connections(
        args: &Options,
        conn_cmd: &ConnectionsCommand,
    ) -> anyhow::Result<Measurements> {
        let mut conns = Vec::new();
        let connections = conn_cmd.connections_increment;

        let mut measurements = Measurements::new();
        'test_loop: loop {
            let start = Instant::now();
            let res = util::establish_connections(&mut conns, args, connections).await;
            if res.is_err() {
                break 'test_loop;
            }

            let elapsed = Instant::now() - start;
            let rate = (connections as f32) / elapsed.as_secs_f32();
            log::info!(
                "Connections currently established: {}, {:.1}/s",
                conns.len(),
                rate
            );

            for conn2 in conns.windows(2) {
                let res = conn2[0].find_node(conn2[1].node_id()).await;
                if res.is_err() {
                    break 'test_loop;
                }
            }

            log::info!("All connections ok.");
            measurements.push(conns.len() as u32, rate);
        }

        Ok(measurements)
    }

    pub async fn test_and_save(
        args: &Options,
        conn_cmd: &ConnectionsCommand,
    ) -> anyhow::Result<()> {
        let measurements = measure_connections(args, conn_cmd).await?;

        if let Some(path) = &args.csv {
            let file = std::fs::File::create(path)?;
            measurements.save(file)?;
        } else {
            measurements.save(std::io::stdout())?;
        }

        Ok(())
    }
}

mod load_test {
    use super::*;

    /// Testable request
    #[derive(Clone, Copy, Debug)]
    enum Request {
        FindNode,
        Neighbours(u32),
        Ping,
    }

    /// Singular test case consisting of repeating a request a set number of times
    #[derive(Clone, Debug)]
    struct Task {
        request: Request,
        count: u32,
    }

    /// Testing plan for a connection
    #[derive(Clone, Debug)]
    struct TaskPlan {
        tasks: Vec<Task>,
    }

    impl TaskPlan {
        fn count_requests(&self) -> u32 {
            self.tasks.iter().map(|task| task.count).sum()
        }
    }

    #[derive(Debug)]
    struct DataPoint {
        connections: u32,
        rate_limit: u32,
        rate: f32,
    }

    #[derive(Debug)]
    struct Measurements {
        data: Vec<DataPoint>,
    }

    impl Measurements {
        fn new() -> Self {
            Measurements { data: Vec::new() }
        }

        fn push(&mut self, connections: u32, rate_limit: u32, rate: f32) {
            self.data.push(DataPoint {
                connections,
                rate_limit,
                rate,
            });
        }

        fn save(&self, writer: impl std::io::Write) -> anyhow::Result<()> {
            let mut w = csv::Writer::from_writer(writer);

            w.write_record(&["connections", "rate-limit", "rate"])?;
            for data_point in &self.data {
                w.write_record(&[
                    &data_point.connections.to_string(),
                    &data_point.rate_limit.to_string(),
                    &data_point.rate.to_string(),
                ])?;
            }

            Ok(())
        }
    }

    /// Block execution to achieve a consistent delay between calls
    async fn limit_rate(start: &mut Instant, delay: Option<f32>) {
        if let Some(secs) = delay {
            let duration = Duration::from_secs_f32(secs);

            loop {
                let now = Instant::now();
                if now > *start + duration {
                    let overshoot = now - (*start + duration);
                    *start = now - overshoot;

                    break;
                }

                let diff = (*start + duration - now) / 3;
                tokio::time::sleep(diff).await;
            }
        }
    }

    async fn run_plan_on_client(
        request_delay: Option<f32>,
        client: &Client,
        plan: &TaskPlan,
    ) -> anyhow::Result<Duration> {
        let start = Instant::now();

        let mut start_limiter = Instant::now();
        for Task { request, count } in &plan.tasks {
            for _ in 0..*count {
                limit_rate(&mut start_limiter, request_delay).await;
                match request {
                    Request::FindNode => {
                        client.find_node(client.node_id()).await?;
                    }
                    Request::Neighbours(n) => {
                        client.neighbours(*n).await?;
                    }
                    Request::Ping => {
                        client.ping_sessions().await;
                    }
                }
            }
        }

        let elapsed = Instant::now() - start;

        Ok(elapsed)
    }

    /// Plan tasks according to the user specification
    fn plan_tasks(load_cmd: &LoadCommand) -> TaskPlan {
        let mut tasks = Vec::new();

        let find_node_weight = load_cmd.find_node_weight;
        let neighbours_weight = load_cmd.neighbours_weight;
        let ping_weight = load_cmd.ping_weight;

        let all_count = find_node_weight + neighbours_weight + ping_weight;
        if all_count == 0 {
            log::error!("At least one request type must have non-zero weight!");
            panic!();
        }

        let scale = load_cmd.requests_per_connection.get() / all_count;

        let find_count = find_node_weight * scale;
        let neighbour_count = neighbours_weight * scale;
        let ping_count = ping_weight * scale;

        tasks.push(Task {
            request: Request::FindNode,
            count: find_count,
        });
        tasks.push(Task {
            request: Request::Neighbours(load_cmd.neighbours_size),
            count: neighbour_count,
        });
        tasks.push(Task {
            request: Request::Ping,
            count: ping_count,
        });

        TaskPlan { tasks }
    }

    /// Load test core
    async fn measure_load(args: &Options, load_cmd: &LoadCommand) -> anyhow::Result<Measurements> {
        let mut measurements = Measurements::new();
        let mut clients = Vec::new();
        for target_connections in &load_cmd.connections {
            // Drop or create connections to reach the target
            if *target_connections > clients.len() {
                for _ in clients.len()..*target_connections {
                    clients.push(util::establish_connection(args).await?);
                }
            } else {
                clients.resize_with(*target_connections, || panic!("logic error"));
            }
            log::info!("Established {} connections", clients.len());

            let plan = plan_tasks(load_cmd);
            let all_requests = plan.count_requests();

            // Append a no-limit entry
            let rate_limits = load_cmd
                .rate_limits
                .iter()
                .map(Some)
                .chain(std::iter::once(None));

            'delay_loop: for rate_limit in rate_limits {
                // Map rate to a corresponding delay
                let delay = rate_limit.map(|r| 1.0 / *r as f32);

                match delay {
                    Some(d) => log::info!("Rate limit: {:.2}.", 1.0 / d),
                    None => log::info!("Unlimited rate."),
                }

                // Run test plans on all connections
                let mut futures = Vec::new();
                for client in &clients {
                    let normalized_delay = delay.map(|t| t * clients.len() as f32);
                    futures.push(run_plan_on_client(normalized_delay, client, &plan));
                }
                let results = join_all(futures.into_iter()).await;

                // Process the results
                let mut request_rates = Vec::new();
                for res in results {
                    let duration = match res {
                        Ok(duration) => duration,
                        Err(e) => {
                            log::info!(
                                "Coulnd't complete with rate limit {:?}\nerror: {}",
                                delay,
                                e.to_string()
                            );
                            measurements.push(clients.len() as u32, *rate_limit.unwrap_or(&0), 0.0);
                            continue 'delay_loop;
                        }
                    };
                    request_rates.push(all_requests as f32 / duration.as_secs_f32());
                }

                let total_rate: f32 = request_rates.iter().sum();
                log::info!("Total requests / sec: {}.", total_rate);
                measurements.push(clients.len() as u32, *rate_limit.unwrap_or(&0), total_rate);
            }
        }

        Ok(measurements)
    }

    /// Run load test and save results
    pub async fn test_and_save(args: &Options, load_cmd: &LoadCommand) -> anyhow::Result<()> {
        let measurements = measure_load(args, load_cmd).await?;

        if let Some(path) = &args.csv {
            let file = std::fs::File::create(path)?;
            measurements.save(file)?;
        } else {
            measurements.save(std::io::stdout())?;
        }

        Ok(())
    }
}

/// Runs the selected test
async fn run() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "trace,mio=info,smoltcp=info".to_string()),
    );
    env_logger::init();

    let args = Options::from_args();

    match &args.command {
        Command::Connections(conn_cmd) => connections_test::test_and_save(&args, conn_cmd).await?,
        Command::Load(load_cmd) => load_test::test_and_save(&args, load_cmd).await?,
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(run()).await
}
