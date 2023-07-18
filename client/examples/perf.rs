use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::prelude::*;

use rand::seq::SliceRandom;
use structopt::{clap, StructOpt};

use ya_relay_client::{Client, ClientBuilder};
use ya_relay_core::crypto::FallbackCryptoProvider;
use ya_relay_core::key::{load_or_generate, Protected};

#[derive(StructOpt)]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
pub struct Options {
    #[structopt(
        short = "a",
        env = "NET_ADDRESS",
        default_value = "udp://127.0.0.1:7464"
    )]
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
    /// Simulates multiple Nodes trying to forward through `ya-relay-server`
    RelayTraffic(RelayingCommand),
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

/// Load test configuration
#[derive(StructOpt)]
pub struct RelayingCommand {
    /// Number of connections to use
    #[structopt(long)]
    connections: usize,
    /// Proportion of find_node calls
    #[structopt(long, default_value = "7")]
    find_node_weight: u32,
    /// Proportion of neighbours calls
    #[structopt(long, default_value = "6")]
    neighbours_weight: u32,
    /// Proportion of ping calls
    #[structopt(long, default_value = "87")]
    ping_weight: u32,
    /// Size of the neighbourhood to request
    #[structopt(long, default_value = "10")]
    neighbours_size: u32,
    /// Test duration
    #[structopt(long, env, parse(try_from_str = humantime::parse_duration), default_value = "5s")]
    pub timeout: Duration,
    /// Artificial limit of requests per second per client.
    #[structopt(long, default_value = "0.037")]
    rate_limit: f64,
    /// Number of clients simulating requestors.
    #[structopt(long, default_value = "1")]
    requestors: usize,
    /// File containing scenario to execute by single requestor
    #[structopt(long)]
    scenario_file: PathBuf,
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

        Ok(client)
    }

    /// Establishes {connections} connections sequentially, appending to the {clients} buffer.
    pub async fn establish_connections(
        clients: &mut Vec<Client>,
        args: &Options,
        num: usize,
    ) -> anyhow::Result<()> {
        let step = 100usize;

        for _target_connections in 0..num {
            clients.push(util::establish_connection(args).await?);

            if clients.len() % step == 0 {
                log::info!("Established {} connections", clients.len());
            }
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

            w.write_record(["connections", "new-connections-rate"])?;
            for data_point in &self.data {
                w.write_record([
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

    pub trait Plan {
        fn count_requests(&self) -> u32;
        fn next(&mut self) -> Option<Request>;
        fn wait(&mut self) -> f32 {
            0.0
        }
    }

    /// Testable request
    #[derive(Clone, Copy, Debug)]
    pub enum Request {
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

    impl Plan for TaskPlan {
        fn count_requests(&self) -> u32 {
            self.tasks.iter().map(|task| task.count).sum()
        }

        fn next(&mut self) -> Option<Request> {
            while !self.tasks.is_empty() {
                if let Some(task) = self.tasks.first_mut() {
                    if task.count > 0 {
                        task.count -= task.count;
                        return Some(task.request);
                    }
                }
                self.tasks.remove(0);
            }

            None
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

    #[derive(Debug)]
    struct PlanRunResult {
        /// Total time taken by the plan execution
        elapsed: Duration,
        /// Fine-grained time spent on .awaiting requests
        busy: Duration,
        /// Number of dropped packets
        dropped: usize,
        /// Connection status
        connection_valid: bool,
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

            w.write_record(["connections", "rate-limit", "rate"])?;
            for data_point in &self.data {
                w.write_record([
                    &data_point.connections.to_string(),
                    &data_point.rate_limit.to_string(),
                    &data_point.rate.to_string(),
                ])?;
            }

            Ok(())
        }
    }

    /// Block execution to achieve a consistent delay between calls
    pub async fn limit_rate(start: &mut Instant, delay: Option<f32>) {
        if let Some(secs) = delay {
            let duration = Duration::from_secs_f32(secs);

            loop {
                let now = Instant::now();
                if now > *start + duration {
                    let overshoot = now - (*start + duration);
                    *start = now.checked_sub(overshoot).unwrap();

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
        plan: TaskPlan,
    ) -> PlanRunResult {
        let start = Instant::now();
        let mut busy = Duration::from_secs_f32(0.0);

        let mut start_limiter = Instant::now();
        let mut dropped = 0;
        let mut connection_valid = true;

        for Task { request, count } in &plan.tasks {
            for _ in 0..*count {
                limit_rate(&mut start_limiter, request_delay).await;

                let req_start = Instant::now();
                match request {
                    Request::FindNode => {
                        let resp = client.find_node(client.node_id()).await;
                        if let Err(e) = resp {
                            dropped += 1;
                            if !e.to_string().contains("timed out") {
                                connection_valid = false;
                            }
                        } else {
                            busy += req_start.elapsed();
                        }
                    }
                    Request::Neighbours(n) => {
                        let resp = client.neighbours(*n).await;
                        if let Err(_e) = resp {
                            dropped += 1;
                        } else {
                            busy += req_start.elapsed();
                        }
                    }
                    Request::Ping => {
                        client.ping_sessions().await;
                        busy += req_start.elapsed();
                    }
                }
            }
        }

        let elapsed = Instant::now() - start;

        PlanRunResult {
            elapsed,
            busy,
            dropped,
            connection_valid,
        }
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

        for target_connections in load_cmd.connections.iter() {
            // Drop or create connections to reach the target
            let mut establish_errors = 0;
            while *target_connections > clients.len() {
                let to_establish = target_connections - clients.len();
                let res = util::establish_connections(&mut clients, args, to_establish).await;
                if res.is_err() {
                    establish_errors += 1;
                }
            }

            if *target_connections < clients.len() {
                clients.resize_with(*target_connections, || panic!("logic error"));
            }

            log::info!("Established {} connections", clients.len());
            log::info!("Failed connections: {}", establish_errors);

            let plan = plan_tasks(load_cmd);
            let all_requests = plan.count_requests();

            // Append a no-limit entry
            let rate_limits = load_cmd.rate_limits.iter().map(Some).chain(None);

            for rate_limit in rate_limits {
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
                    let mut random_plan = plan.clone();
                    random_plan.tasks.shuffle(&mut rand::thread_rng());
                    futures.push(run_plan_on_client(normalized_delay, client, random_plan));
                }
                let results = future::join_all(futures.into_iter()).await;

                // Process the results
                let mut request_rates = Vec::new();
                let mut busy_request_times = Vec::new();
                let mut valid_mask = Vec::new();
                let mut dropped_all = 0;
                for PlanRunResult {
                    elapsed,
                    busy,
                    dropped,
                    connection_valid,
                } in results.into_iter()
                {
                    request_rates.push(all_requests as f32 / elapsed.as_secs_f32());
                    busy_request_times
                        .push(busy.as_secs_f32() / all_requests as f32 / clients.len() as f32);
                    dropped_all += dropped;
                    valid_mask.push(connection_valid);
                }

                clients = clients
                    .into_iter()
                    .zip(valid_mask.into_iter())
                    .filter(|(_client, mask)| *mask)
                    .map(|(client, _mask)| client)
                    .collect();

                let total_rate: f32 = request_rates.iter().sum();
                log::info!("Total requests / sec: {}.", total_rate);

                let total_busy: f32 = busy_request_times.iter().sum();
                log::info!("Average request-response delay: {}s", total_busy);

                log::info!("Dropped packets: {}", dropped_all);

                log::info!("Connections after filtering dropped: {}", clients.len());

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

mod relaying {
    use super::*;
    use crate::load_test::{limit_rate, Plan, Request};

    use anyhow::{anyhow, bail};
    use rand::distributions::{Distribution, Uniform};
    use serde::{Deserialize, Serialize};
    use std::collections::{HashMap, HashSet};
    use std::fs::File;
    use std::ops::Add;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;
    use tokio_stream::wrappers::UnboundedReceiverStream;
    use ya_relay_client::proto::Payload;
    use ya_relay_core::NodeId;

    #[derive(Clone, Debug, Default, Serialize)]
    pub struct Stats {
        duration: Duration,
        calls: usize,
        dropped: usize,
        find_node: usize,
        neighborhood: usize,
        ping: usize,
    }

    impl Add for Stats {
        type Output = Self;

        fn add(mut self, other: Self) -> Self {
            self.dropped += other.dropped;
            self.calls += other.calls;
            self.duration = std::cmp::max(self.duration, other.duration);
            self.neighborhood += other.neighborhood;
            self.ping += other.ping;
            self.find_node += other.find_node;
            self
        }
    }

    impl Stats {
        fn save(&self, writer: impl std::io::Write) -> anyhow::Result<()> {
            let mut w = csv::Writer::from_writer(writer);

            w.write_record([
                "calls",
                "dropped-calls",
                "calls-per-second",
                "dropped-per-second",
                "find-node",
                "ping",
                "neighborhood",
            ])?;
            w.write_record(&[
                self.calls.to_string(),
                self.dropped.to_string(),
                (self.calls / self.duration.as_secs() as usize).to_string(),
                (self.dropped / self.duration.as_secs() as usize).to_string(),
                self.find_node.to_string(),
                self.ping.to_string(),
                self.neighborhood.to_string(),
            ])?;

            Ok(())
        }
    }

    /// Generates tasks with demanded frequency.
    #[derive(Clone, Debug)]
    pub struct FrequencyPlan {
        tasks: Vec<FrequencyTask>,
        end: Arc<AtomicBool>,
        delay: Duration,
    }

    #[derive(Clone, Debug)]
    pub struct FrequencyTask {
        request: Request,
        freq: u32,
    }

    impl Plan for FrequencyPlan {
        /// Requests per second. Overall number of requests is infinite.
        fn count_requests(&self) -> u32 {
            self.tasks.iter().map(|task| task.freq).sum()
        }

        fn next(&mut self) -> Option<Request> {
            if self.end.load(SeqCst) {
                return None;
            }

            let range = self.count_requests();

            let between = Uniform::from(0..range);
            let mut rng = rand::thread_rng();

            let sample = between.sample(&mut rng);
            let mut start_range = 0u32;

            for task in self.tasks.iter() {
                if (start_range..start_range + task.freq).contains(&sample) {
                    return Some(task.request);
                }

                start_range += task.freq;
            }

            None
        }

        fn wait(&mut self) -> f32 {
            self.delay.as_secs_f32()
        }
    }

    impl FrequencyPlan {
        pub fn finish(&mut self) {
            self.end.store(true, SeqCst);
        }
    }

    #[allow(dead_code)]
    #[derive(Clone, Debug, Deserialize)]
    struct Record {
        op_type: String,
        timestamp: f64,
        reliable: String,
        node_id: u64,
        remote: u64,
        session: u64,
        size: u64,
    }

    #[allow(dead_code)]
    #[derive(Clone, Debug)]
    struct Action {
        timestamp: f64,
        reliable: bool,
        remote_id: NodeId,
        size: u64,
    }

    #[derive(Clone)]
    struct Scenario {
        actions: Vec<Action>,
        requestor: Client,
        providers: HashMap<NodeId, Client>,
    }

    #[derive(Clone, Debug, Deserialize, derive_more::Display)]
    #[display(
        fmt = "Provider: {}: {} expected={}, received={}",
        id,
        "self.is_ok()",
        expected,
        received
    )]
    struct ProviderStat {
        id: NodeId,
        expected: usize,
        received: usize,
    }

    impl ProviderStat {
        pub fn is_ok(&self) -> bool {
            self.received == self.expected
        }
    }

    impl Scenario {
        pub fn transmission(&self, id: NodeId) -> usize {
            self.actions
                .iter()
                .filter_map(|action| {
                    if action.remote_id == id {
                        Some(action.size as usize)
                    } else {
                        None
                    }
                })
                .sum()
        }
    }

    fn load_scenario(file: &Path) -> anyhow::Result<Vec<Record>> {
        log::info!("Loading scenarios from: {}", file.display());

        let mut rdr = csv::Reader::from_reader(File::open(file)?);
        rdr.deserialize()
            .map(|result| result.map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>, _>>()
    }

    fn create_scenario(
        records: Vec<Record>,
        clients: &mut Vec<Client>,
    ) -> anyhow::Result<Scenario> {
        let ids = records
            .iter()
            .map(|record| record.node_id)
            .collect::<HashSet<u64>>()
            .into_iter()
            .collect::<Vec<_>>();

        if ids.len() + 1 > clients.len() {
            bail!(
                "Need more connections to create scenario, available: {}, expected: {}",
                clients.len(),
                ids.len() + 1
            );
        }

        let requestor = clients.remove(0);
        let providers = clients.drain(0..ids.len()).collect::<Vec<_>>();
        let ids_map: HashMap<u64, NodeId> = providers
            .iter()
            .enumerate()
            .map(|(idx, client)| (ids[idx], client.node_id()))
            .collect();

        let records = records
            .into_iter()
            .map(|record| Action {
                timestamp: record.timestamp,
                reliable: record.reliable.contains('R'),
                remote_id: ids_map[&record.node_id],
                size: record.size,
            })
            .collect::<Vec<Action>>();

        Ok(Scenario {
            actions: records,
            requestor,
            providers: providers
                .into_iter()
                .map(|provider| (provider.node_id(), provider))
                .collect(),
        })
    }

    pub async fn test_and_save(args: &Options, cmd: &RelayingCommand) -> anyhow::Result<()> {
        let stats = run(args, cmd).await?;

        if let Some(path) = &args.csv {
            let file = std::fs::File::create(path)?;
            stats.save(file)?;
        } else {
            //println!("{}", serde_json::to_string_pretty(&stats)?);
            stats.save(std::io::stdout())?;
        }

        Ok(())
    }

    async fn run(args: &Options, cmd: &RelayingCommand) -> anyhow::Result<Stats> {
        let mut clients = Vec::new();
        util::establish_connections(&mut clients, args, cmd.connections)
            .await
            .map_err(|e| anyhow!("Establishing connections step failed with error: {e}"))?;

        let mut plan = plan_tasks(cmd);

        let mut futures = Vec::new();
        for client in &clients {
            futures.push(generate_idle_traffic(client.clone(), plan.clone()));
        }

        log::info!("Preparing scenarios");

        let records = load_scenario(&cmd.scenario_file)?;
        let scenarios = (0..cmd.requestors)
            .map(|_| create_scenario(records.clone(), &mut clients))
            .collect::<Result<Vec<Scenario>, _>>()?;

        log::info!("Running idle discovery calls.");
        let idle = tokio::task::spawn_local(async move { future::join_all(futures.into_iter()).await });

        log::info!("Running {} scenario(s) (Requestor(s))", scenarios.len());

        let scenarios_results = run_scenarios(scenarios).await?;
        let mut failures = 0;

        for result in &scenarios_results {
            if !result.is_ok() {
                failures += 1;
                log::info!("{result}");
            } else {
                log::debug!("{result}");
            }
        }

        if failures != 0 {
            log::error!("{failures} failures");
        } else {
            log::info!("All transfers successful");
        }

        plan.finish();
        let results = idle.await?;

        results
            .into_iter()
            .reduce(|acc, x| acc + x)
            .ok_or_else(|| anyhow!("Reduce Stats failed"))
    }

    async fn run_scenarios(scenarios: Vec<Scenario>) -> anyhow::Result<Vec<ProviderStat>> {
        let futures = scenarios.into_iter().map(|scenario| {
            async move {
                tokio::time::sleep(random_delay(4000)).await;
                anyhow::Ok(run_scenario(scenario).await?)
            }
            .boxed_local()
        });
        future::join_all(futures)
            .await
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .reduce(|mut acc, item| {
                acc.extend(item.into_iter());
                acc
            })
            .ok_or_else(|| anyhow!("No items??"))
    }

    fn random_delay(max_millis: u64) -> Duration {
        let between = Uniform::from(0..max_millis);
        let mut rng = rand::thread_rng();

        Duration::from_millis(between.sample(&mut rng))
    }

    async fn run_scenario(scenarios: Scenario) -> anyhow::Result<Vec<ProviderStat>> {
        let requestor = scenarios.requestor.clone();
        let _receiver = requestor.forward_receiver().await.ok_or_else(|| {
            anyhow!(
                "Failed to get receiver for Requestor: {}",
                requestor.node_id()
            )
        })?;

        let provider_futures = scenarios
            .providers
            .clone()
            .into_iter()
            .map(|(id, provider)| {
                let expected = scenarios.transmission(id);
                async move { anyhow::Ok((id, run_provider(provider, expected).await?)) }
                    .boxed_local()
            })
            .collect::<Vec<_>>();

        let providers_handle =
            tokio::task::spawn_local(async move { future::join_all(provider_futures.into_iter()).await });

        // TODO: Should be asynchronous, we can't wait for operation finish.
        let start = tokio::time::Instant::now();
        for action in &scenarios.actions {
            let delay = start + Duration::from_millis(action.timestamp as u64);
            tokio::time::sleep_until(delay).await;

            log::info!("Sending {} bytes to: {}", action.size, action.remote_id);

            let data = (0..action.size).map(|n| n as u8).collect();

            let mut sender = requestor.forward(action.remote_id).await?;
            sender.send(Payload::Vec(data)).await.ok();
        }

        Ok(providers_handle
            .await?
            .into_iter()
            .collect::<Result<HashMap<NodeId, Arc<AtomicUsize>>, _>>()?
            .into_iter()
            .map(|(id, size)| ProviderStat {
                id,
                received: size.load(SeqCst),
                expected: scenarios.transmission(id),
            })
            .collect())
    }

    async fn run_provider(client: Client, expected: usize) -> anyhow::Result<Arc<AtomicUsize>> {
        let received = Arc::new(AtomicUsize::new(0));
        let rx = client
            .forward_receiver()
            .await
            .ok_or_else(|| anyhow!("Failed to get receiver for Provider: {}", client.node_id()))?;

        let (finish_tx, mut finish_rx) = tokio::sync::mpsc::channel(1);

        let received_ = received.clone();
        tokio::task::spawn_local(async move {
            UnboundedReceiverStream::new(rx)
                .for_each(|item| {
                    let received = received_.clone();
                    let finish_tx = finish_tx.clone();
                    async move {
                        let payload_len = item.payload.len();
                        let sum = received.fetch_add(payload_len, SeqCst) + payload_len;
                        if sum == expected {
                            finish_tx.send(()).await.ok();
                        }
                    }
                })
                .await;
        });

        finish_rx.recv().await;
        Ok(received)
    }

    async fn generate_idle_traffic(client: Client, mut plan: impl Plan) -> Stats {
        let mut calls: usize = 0;
        let mut dropped: usize = 0;
        let mut find_node: usize = 0;
        let mut ping: usize = 0;
        let mut neighborhood: usize = 0;

        let start = Instant::now();

        let mut start_limiter = Instant::now();
        while let Some(request) = plan.next() {
            limit_rate(&mut start_limiter, Some(plan.wait())).await;

            match request {
                Request::FindNode => {
                    find_node += 1;
                    if client.find_node(client.node_id()).await.is_err() {
                        dropped += 1;
                    }
                }
                Request::Neighbours(n) => {
                    neighborhood += 1;
                    if client.neighbours(n).await.is_err() {
                        dropped += 1;
                    }
                }
                Request::Ping => {
                    ping += 1;
                    client.ping_sessions().await;
                }
            }
            calls += 1;
        }

        let elapsed = Instant::now() - start;
        Stats {
            duration: elapsed,
            calls,
            dropped,
            find_node,
            neighborhood,
            ping,
        }
    }

    /// Plan tasks according to the user specification
    fn plan_tasks(cmd: &RelayingCommand) -> FrequencyPlan {
        let mut tasks = Vec::new();

        let all_count = cmd.find_node_weight + cmd.neighbours_weight + cmd.ping_weight;
        if all_count == 0 {
            log::error!("At least one request type must have non-zero weight!");
            panic!();
        }

        tasks.push(FrequencyTask {
            request: Request::FindNode,
            freq: cmd.find_node_weight,
        });
        tasks.push(FrequencyTask {
            request: Request::Neighbours(cmd.neighbours_size),
            freq: cmd.neighbours_weight,
        });
        tasks.push(FrequencyTask {
            request: Request::Ping,
            freq: cmd.ping_weight,
        });

        FrequencyPlan {
            tasks,
            end: Arc::new(AtomicBool::new(false)),
            delay: Duration::from_secs_f64(1f64 / cmd.rate_limit),
        }
    }
}

/// Runs the selected test
async fn run() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG")
            .unwrap_or_else(|_| "info,ya_relay_core=warn,ya_relay_client=warn".to_string()),
    );

    env_logger::init();

    let args = Options::from_args();

    match &args.command {
        Command::Connections(conn_cmd) => connections_test::test_and_save(&args, conn_cmd).await?,
        Command::Load(load_cmd) => load_test::test_and_save(&args, load_cmd).await?,
        Command::RelayTraffic(cmd) => relaying::test_and_save(&args, cmd).await?,
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(run()).await
}
