use futures::future::{AbortHandle, Abortable};
use futures::FutureExt;
use url::Url;

use crate::server::Server;

pub struct ServerWrapper {
    pub server: Server,
    handle: AbortHandle,
}

pub async fn init_test_server() -> anyhow::Result<ServerWrapper> {
    // Initialize logger for all tests. Thi function will be called multiple times,
    // so we `try_init`.
    let _ = env_logger::builder().try_init();

    let server = Server::bind_udp(Url::parse("udp://0.0.0.0:0")?).await?;

    let (abort_handle, abort_registration) = AbortHandle::new_pair();

    tokio::task::spawn_local(Abortable::new(
        server.clone().run().map(|result| {
            if let Err(e) = result {
                log::error!("Server error: {}", e);
            }
        }),
        abort_registration,
    ));

    Ok(ServerWrapper {
        server,
        handle: abort_handle,
    })
}

impl Drop for ServerWrapper {
    fn drop(&mut self) {
        self.server.inner.socket.clone().close_channel();
        self.handle.abort();

        log::debug!("[TEST] Dropping ServerWrapper.");
    }
}
