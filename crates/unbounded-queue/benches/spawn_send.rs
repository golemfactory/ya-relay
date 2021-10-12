use criterion::{BenchmarkId, Criterion};
use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, StreamExt};
use rand::Rng;
use std::future::Future;

async fn spawn_send(size: usize) {
    let (mtx, mut mrx) = mpsc::channel(8);
    let (tx, rx) = oneshot::channel();

    let mut rng = rand::thread_rng();
    let data: Vec<u64> = (0..128).map(|_| rng.gen()).collect();

    tokio::task::spawn_local(async move {
        for _ in 0..size {
            let mut mtx = mtx.clone();
            let data = data.clone();
            tokio::task::spawn_local(async move {
                let _ = mtx.send(data).await;
            });
        }
    });

    tokio::task::spawn_local(async move {
        let mut counter: usize = 0;
        while let Some(_) = mrx.next().await {
            counter += 1;
            if counter >= size {
                let _ = tx.send(());
                break;
            }
        }
    });

    if let Err(_) = rx.await {
        eprintln!("Cancelled");
    }
}

async fn queue(size: usize) {
    let (mut qtx, mut qrx) = unbounded_queue::queue_with_capacity(8);
    let (tx, rx) = oneshot::channel();

    let mut rng = rand::thread_rng();
    let data: Vec<u64> = (0..128).map(|_| rng.gen()).collect();

    tokio::task::spawn_local(async move {
        for _ in 0..size {
            let data = data.clone();
            qtx.send(data).unwrap();
        }
    });

    tokio::task::spawn_local(async move {
        let mut counter: usize = 0;
        while let Some(_) = qrx.next().await {
            counter += 1;
            if counter >= size {
                let _ = tx.send(());
                break;
            }
        }
    });

    if let Err(_) = rx.await {
        eprintln!("Cancelled");
    }
}

fn benchmark(c: &mut Criterion) {
    let size: usize = 1000;

    fn runtime() -> impl criterion::async_executor::AsyncExecutor {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn local_set<O>(f: impl Future<Output = O>) -> impl Future<Output = O> {
        let local = tokio::task::LocalSet::new();
        async move { local.run_until(f).await }
    }

    c.bench_with_input(BenchmarkId::new("channels", size), &size, |b, &s| {
        b.to_async(runtime()).iter(|| local_set(spawn_send(s)));
    });
    c.bench_with_input(BenchmarkId::new("queue", size), &size, |b, &s| {
        b.to_async(runtime()).iter(|| local_set(queue(s)));
    });
}

criterion::criterion_group!(benches, benchmark);
criterion::criterion_main!(benches);
