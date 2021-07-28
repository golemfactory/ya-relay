use futures::FutureExt;
use url::Url;

use crate::server::Server;

pub async fn init_test_server() -> anyhow::Result<Server> {
    // Initialize logger for all tests. Thi function will be called multiple times,
    // so we `try_init`.
    let _ = env_logger::builder().try_init();

    let server = Server::bind_udp(Url::parse("udp://127.0.0.1:8888")?).await?;

    tokio::task::spawn_local(server.clone().run().map(|result| {
        if let Err(e) = result {
            log::error!("Server error: {}", e);
        }
    }));

    Ok(server)
}
