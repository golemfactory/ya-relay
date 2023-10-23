use std::error::Error;
use std::hint::black_box;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use anyhow::bail;
use tokio::net::UdpSocket;
use ya_relay_proto::codec::BytesMut;
use ya_relay_server::udp_server::UdpServerBuilder;


#[test]
fn test_me() -> Result<(), Box<dyn Error>> {
    use std::thread;
    
    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));
    let stop = 1000;
    
    let th1 = {
        let a = a.clone();
        let b= b.clone();
        thread::spawn(move || {
            let mut n = 0;
            loop {
                let v = a.fetch_add(1, Ordering::Relaxed);
                if v >= stop {
                    break
                }
                while b.load(Ordering::Relaxed) < v {
                    black_box({
                        n += 1;
                    })
                }
                eprintln!("A step {v} {n:10} a={}, b={}", a.load(Ordering::Relaxed), b.load(Ordering::Relaxed));
            }
        })
    };

    let th2= {
        let a = a.clone();
        let b = b.clone();
        thread::spawn(move || {
            let mut n = 0;
            loop {
                let v = b.fetch_add(1, Ordering::Relaxed);
                if v >= stop {
                    break
                }
                while a.load(Ordering::Relaxed) < v {
                    black_box({
                        n += 1;
                    })
                }
                eprintln!("B step {v} {n:10} a={}, b={}", a.load(Ordering::Relaxed), b.load(Ordering::Relaxed));
            }
        })
    };

    th1.join().unwrap();
    th2.join().unwrap();


    eprintln!("done a={a:?} b={b:?}");
    Ok(())
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    log::info!("start");
    let g = Arc::new(());
    let _ = UdpServerBuilder::new(move |reply: Rc<UdpSocket>| {
        let g = g.clone();
        if Arc::strong_count(&g) > 4 {
            bail!("failed to start");
        }
        else {
            log::info!("cnt = {}", Arc::strong_count(&g));
        }
        let reply = Rc::downgrade(&reply);
        Ok(move |packet: BytesMut, src| {
            let _g = g.clone();
            let reply = reply.clone();

            async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Some(reply) = reply.upgrade() {
                    let _v = reply.send_to(&packet, src).await?;
                }
                Ok(())
            }
        })
    })
    .max_tasks_per_worker(8)
    .workers(10)
    .start("0.0.0.0:4455".parse()?).await?;

    futures::future::pending::<()>().await;
    Ok(())
}
