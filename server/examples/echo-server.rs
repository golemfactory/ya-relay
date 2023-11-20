use std::rc::Rc;

use std::sync::Arc;
use std::time::Duration;
use ya_relay_proto::codec::BytesMut;
use ya_relay_server::udp_server::{worker_fn, UdpServerBuilder, UdpSocket};

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    log::info!("start");
    let g = Arc::new(());
    let _ = UdpServerBuilder::new(move |reply: Rc<UdpSocket>| {
        let g = g.clone();

        let reply = Rc::downgrade(&reply);
        worker_fn(move |packet: BytesMut, src| {
            let _g = g.clone();
            let reply = reply.clone();

            Ok(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Some(reply) = reply.upgrade() {
                    let _v = reply.send_to(&packet, src).await?;
                }
                Ok(())
            })
        })
    })
    .max_tasks_per_worker(8)
    .workers(10)
    .start("0.0.0.0:4455".parse()?)
    .await?;

    futures::future::pending::<()>().await;
    Ok(())
}
