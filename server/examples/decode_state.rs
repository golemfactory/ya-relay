mod common;

use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;
use ya_relay_server::{Selector, SessionManager};

use common::is_public_ip;

#[derive(Parser)]
#[command(about = "List nodes with a public IP address from a relay state file")]
struct Args {
    /// Path to the sessions_v2.state file.
    #[arg(default_value = "sessions_v2.state")]
    state_file: PathBuf,
}

struct Row {
    node_id: String,
    ip: IpAddr,
    port: u16,
    address_valid: bool,
    session_id: String,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let session_manager = SessionManager::load(&args.state_file)?;
    let mut rows = Vec::new();

    for (node_id, sessions) in session_manager.nodes_for(Selector::All, usize::MAX) {
        for session in sessions.into_iter().filter_map(|session| session.upgrade()) {
            let ip = session.peer.ip();
            if is_public_ip(ip) {
                rows.push(Row {
                    node_id: node_id.to_string(),
                    ip,
                    port: session.peer.port(),
                    address_valid: session.addr_status.lock().is_valid(),
                    session_id: session.session_id.to_string(),
                });
            }
        }
    }

    rows.sort_unstable_by(|a, b| {
        (&a.node_id, a.ip, a.port, &a.session_id).cmp(&(&b.node_id, b.ip, b.port, &b.session_id))
    });

    println!("node_id\tip\tport\taddress_valid\tsession_id");
    for row in &rows {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            row.node_id, row.ip, row.port, row.address_valid, row.session_id
        );
    }
    eprintln!("Found {} node session(s) with a public IP", rows.len());

    Ok(())
}
