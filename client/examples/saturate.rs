use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use log::LevelFilter;
use prettytable::format::consts::FORMAT_BOX_CHARS;
use prettytable::format::TableFormat;
use prettytable::{Attr, Cell, Row, Table};
use structopt::{clap, StructOpt};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tokio_stream::wrappers::UnboundedReceiverStream;

use ya_relay_client::client::ForwardSender;
use ya_relay_client::{ClientBuilder, ForwardReceiver};
use ya_relay_core::crypto::FallbackCryptoProvider;
use ya_relay_core::key::{load_or_generate, Protected};
use ya_relay_core::session::TransportType;
use ya_relay_core::NodeId;

#[derive(StructOpt)]
#[structopt(about = "Client performance test")]
#[structopt(rename_all = "kebab-case")]
#[structopt(global_setting = clap::AppSettings::ColoredHelp)]
struct Cli {
    /// Address to bind to
    #[structopt(
        short = "l",
        env = "YA_NET_BIND_URL",
        default_value = "udp://0.0.0.0:0"
    )]
    listen: url::Url,
    #[structopt(
        short = "r",
        env = "YA_NET_RELAY_HOST",
        default_value = "udp://52.17.188.4:7477"
    )]
    relay: url::Url,
    /// Name used log / csv filenames
    #[structopt(short, long)]
    name: Option<String>,
    /// Private key file
    #[structopt(short = "f", long, env = "CLIENT_KEY_FILE")]
    key_file: Option<String>,
    /// Private key password
    #[structopt(short = "p", long, env = "CLIENT_KEY_PASSWORD", parse(from_str = Protected::from))]
    key_password: Option<Protected>,
    /// Set max TCP receive buffer size
    #[structopt(long, env = "YA_NET_TCP_MAX_RECV_BUF_SIZE")]
    tcp_max_recv_buf_size: Option<usize>,
    /// Set max TCP send buffer size
    #[structopt(long, env = "YA_NET_TCP_MAX_SEND_BUF_SIZE")]
    tcp_max_send_buf_size: Option<usize>,
    /// Sent chunk size
    #[structopt(short = "c", long, default_value = "65536")]
    chunk_size: usize,
    /// Print state every N seconds
    #[structopt(long, parse(try_from_str = humantime::parse_duration), default_value = "250ms")]
    interval: Duration,
    /// Log / CSV output directory
    #[structopt(short = "d", long)]
    work_dir: Option<PathBuf>,

    #[structopt(subcommand)]
    command: Command,
}

#[derive(StructOpt, Clone, Debug)]
#[structopt(rename_all = "kebab-case")]
enum Command {
    /// Await for connections
    Listen {
        #[structopt(short, long, parse(try_from_str = humantime::parse_duration))]
        sample: Option<Duration>,
    },
    /// Connect to node
    Connect {
        node_id: String,
        #[structopt(short, long, parse(try_from_str = humantime::parse_duration))]
        time: Option<Duration>,
    },
}

#[derive(Clone, Copy, Default)]
struct Stats {
    rx: f32,
    tx: f32,
    rx_total: usize,
    tx_total: usize,
}

struct NodeStats {
    started: Instant,
    reliable: Stats,
    #[allow(unused)]
    unreliable: Stats,
}

impl Default for NodeStats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            reliable: Default::default(),
            unreliable: Default::default(),
        }
    }
}

#[derive(Clone)]
struct State {
    inner: Arc<RwLock<BTreeMap<String, NodeStats>>>,
    err: Arc<RwLock<Option<anyhow::Error>>>,
    chunk_size: usize,
    log_file: PathBuf,
}

impl State {
    pub fn new(chunk_size: usize, log_file: PathBuf) -> Self {
        Self {
            inner: Default::default(),
            err: Default::default(),
            chunk_size,
            log_file,
        }
    }

    pub async fn is_err(&self) -> bool {
        let err = self.err.read().await;
        err.is_some()
    }

    pub async fn set_err(&self, error: anyhow::Error) {
        let mut err = self.err.write().await;
        err.replace(error);
    }

    pub async fn print_err(&self) {
        let err = self.err.read().await;
        if let Some(ref e) = *err {
            eprintln!("Error: {e:?}");
        }
    }
}

fn receive(receiver: ForwardReceiver, state: State) -> impl Future<Output = ()> + 'static {
    UnboundedReceiverStream::new(receiver).for_each(move |fwd| {
        let state = state.clone();
        async move {
            let mut inner = state.inner.write().await;
            let node_stats = inner.entry(fwd.node_id.to_string()).or_default();
            let stats = if fwd.transport == TransportType::Unreliable {
                &mut node_stats.unreliable
            } else {
                &mut node_stats.reliable
            };

            let now = Instant::now();
            let dt = now - node_stats.started;

            stats.rx_total += fwd.payload.len();
            stats.rx = if dt.as_secs() >= 1 {
                (stats.rx_total as f32) / dt.as_secs_f32()
            } else {
                0.
            };
        }
    })
}

fn send(
    mut sender: ForwardSender,
    node_id: NodeId,
    state: State,
) -> impl Future<Output = ()> + 'static {
    let chunk_size = state.chunk_size;
    let node_id = node_id.to_string();

    let cc = tokio::signal::ctrl_c();
    let send = async move {
        loop {
            if state.is_err().await {
                break;
            }

            if let Err(e) = sender.send(vec![1; chunk_size].into()).await {
                state.set_err(anyhow!(e)).await;
                break;
            }

            let mut inner = state.inner.write().await;
            let mut node_stats = inner.entry(node_id.clone()).or_default();

            let now = Instant::now();
            let dt = now - node_stats.started;

            node_stats.reliable.tx_total += chunk_size;
            node_stats.reliable.tx = if dt.as_secs() >= 1 {
                (node_stats.reliable.tx_total as f32) / dt.as_secs_f32()
            } else {
                0.
            };
        }
    };

    async move {
        futures::pin_mut!(send);
        futures::pin_mut!(cc);
        let _ = futures::future::select(send, cc).await;
    }
}

async fn print_state(node_id: NodeId, state: State, delay: Duration) {
    let headers = ["node id", "socket", "time", "rx", "rx sum", "tx", "tx sum"];

    let cc = tokio::signal::ctrl_c();
    let print = async move {
        loop {
            let values = {
                let inner = state.inner.read().await;
                inner
                    .iter()
                    .flat_map(|(node_id, stats)| {
                        let time = Duration::new((Instant::now() - stats.started).as_secs(), 0);

                        vec![
                            vec![
                                node_id.to_string(),
                                "tcp".to_string(),
                                humantime::format_duration(time).to_string(),
                                bytesize_per_second(stats.reliable.rx as u64),
                                bytesize::to_string(stats.reliable.rx_total as u64, false),
                                bytesize_per_second(stats.reliable.tx as u64),
                                bytesize::to_string(stats.reliable.tx_total as u64, false),
                            ],
                            vec![
                                node_id.to_string(),
                                "".to_string(),
                                humantime::format_duration(time).to_string(),
                                bytesize_per_second(stats.unreliable.rx as u64),
                                bytesize::to_string(stats.unreliable.rx_total as u64, false),
                                bytesize_per_second(stats.unreliable.tx as u64),
                                bytesize::to_string(stats.unreliable.tx_total as u64, false),
                            ],
                        ]
                    })
                    .collect()
            };

            if state.is_err().await {
                break;
            }

            print!("\x1B[2J\x1B[1;1H");
            println!("node:\t {}", node_id);
            println!("log:\t {}", state.log_file.display());
            println!("\n{}, every {}s", Utc::now(), delay.as_secs_f32());
            print_table(&headers, values, *FORMAT_BOX_CHARS);

            tokio::time::sleep(delay).await;
        }
    };

    futures::pin_mut!(print);
    futures::pin_mut!(cc);
    let _ = futures::future::select(print, cc).await;
}

async fn sample_state(node_id: NodeId, state: State, delay: Duration, time: Duration) {
    let deadline = Instant::now() + time;
    let csv_file = state.log_file.with_extension("csv");
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&csv_file)
        .await
        .expect("Unable to open the CSV file");

    print!("\x1B[2J\x1B[1;1H");
    println!("node:\t {}", node_id);
    println!("log:\t {}", state.log_file.display());
    println!("csv:\t {}", csv_file.display());

    loop {
        let values: Vec<Vec<String>> = {
            let inner = state.inner.read().await;
            inner
                .iter()
                .map(|(node_id, stats)| {
                    let time =
                        Duration::from_secs_f32((Instant::now() - stats.started).as_secs_f32());
                    vec![
                        time.as_secs_f32().to_string(),
                        node_id.to_string(),
                        (stats.reliable.rx as u64).to_string(),
                        (stats.reliable.rx_total as u64).to_string(),
                    ]
                })
                .collect()
        };

        for value in values {
            output.write(value.join(",").as_bytes()).await.unwrap();
            output.write(b"\n").await.unwrap();
        }

        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(delay).await;
    }

    let _ = output.flush().await;
    println!("... done.");
}

#[inline]
fn bytesize_per_second(value: u64) -> String {
    format!("{} /s", bytesize::to_string(value as u64, false))
}

fn print_table(headers: &[&str], values: Vec<Vec<String>>, table_format: TableFormat) {
    let mut table = Table::new();
    table.set_format(table_format);
    table.set_titles(Row::new(
        headers
            .iter()
            .map(|s| Cell::new(s).with_style(Attr::Bold))
            .collect(),
    ));

    if values.is_empty() {
        let row = Row::new((0..headers.len()).map(|_| Cell::new("")).collect());
        table.add_row(row);
    } else {
        for row_values in values {
            let row = Row::new(row_values.iter().map(|s| Cell::new(s.as_str())).collect());
            table.add_row(row);
        }
    }

    let _ = table.printstd();
}

async fn run() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    std::env::set_var(
        "RUST_LOG",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "trace,mio=info,smoltcp=info".to_string()),
    );

    let cli: Cli = Cli::from_args();
    let mut builder = if let Some(ref key_file) = cli.key_file {
        let password = cli.key_password.clone();
        let secret = load_or_generate(key_file, password);
        ClientBuilder::from_url(cli.relay)
            .crypto(FallbackCryptoProvider::new(secret))
            .listen(cli.listen)
    } else {
        ClientBuilder::from_url(cli.relay).listen(cli.listen)
    };

    if let Some(max) = cli.tcp_max_recv_buf_size {
        builder = builder.tcp_max_recv_buffer_size(max)?;
    }
    if let Some(max) = cli.tcp_max_send_buf_size {
        builder = builder.tcp_max_send_buffer_size(max)?;
    }

    let mut client = builder.build().await?;
    let node_id = client.node_id();

    let name = match cli.name {
        Some(name) => name,
        None => format!("ya-relay-saturate-{}.log", node_id),
    };
    let work_dir = match cli.work_dir {
        Some(dir) => dir,
        None => std::env::temp_dir(),
    };

    tokio::fs::create_dir_all(&work_dir)
        .await
        .expect("Failed to create working directory");
    let log_file = work_dir.join(format!("{}.log", name));

    let chunk_size = 1_usize.max(cli.chunk_size);
    let state = State::new(chunk_size, log_file.clone());
    simple_logging::log_to_file(log_file.clone(), LevelFilter::Info)?;

    let address = client.bind_addr().await?;
    let receiver = client.forward_receiver().await.unwrap();
    tokio::task::spawn(receive(receiver, state.clone()));

    let _ = client.sessions.server_session().await?;
    println!("Client {} is listening on {}", node_id, address);

    match cli.command {
        Command::Listen { sample } => {
            println!("Awaiting incoming connections");

            if let Some(sample) = sample {
                sample_state(node_id, state.clone(), cli.interval, sample).await;
            } else {
                tokio::task::spawn(print_state(node_id, state.clone(), cli.interval));
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        Command::Connect { node_id, time } => {
            println!("Connecting to {}", node_id);

            let node_id = NodeId::from_str(node_id.as_str()).context("Invalid NodeId")?;
            let sender = client.forward(node_id).await?;

            tokio::task::spawn(print_state(node_id, state.clone(), cli.interval));

            if let Some(time) = time {
                let _ = tokio::time::timeout(time, send(sender, node_id, state.clone())).await;
            } else {
                tokio::task::spawn(send(sender, node_id, state.clone()));
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    state.print_err().await;
    client.shutdown().await?;

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(run()).await
}
