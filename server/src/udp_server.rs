//!
//!
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use actix_rt::Arbiter;
use bytes::BytesMut;
use futures::prelude::*;
use metrics::{Key, Label, Unit};

pub use socket::{PacketType, UdpSocket, UdpSocketConfig};

use crate::metrics::InstanceCountGuard;

mod socket;

static KEY_UDP_SERVER_WORKERS: &str = "udp-server.workers";

pub trait WorkerFactory {
    type Worker: Worker;

    fn new_worker(&self, reply: Rc<UdpSocket>) -> anyhow::Result<Self::Worker>;
}

pub trait Worker {
    type Fut: Future<Output = anyhow::Result<()>>;

    fn handle(&self, request: BytesMut, src: SocketAddr, pt: PacketType) -> Self::Fut;
}

pub struct UdpServerBuilder<F> {
    factory: F,
    workers: usize,
    max_tasks_per_worker: usize,
    max_packet_size: usize,
}

pub struct UdpServer {
    bind_addr: SocketAddr,
    arbiters: Vec<Arbiter>,
}

impl<F: WorkerFactory + Sync + Send + 'static> UdpServerBuilder<F> {
    pub fn new(factory: F) -> Self {
        Self {
            factory,
            workers: 8,
            max_tasks_per_worker: 32,
            max_packet_size: 0x8000,
        }
    }

    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    pub fn max_tasks_per_worker(mut self, max_tasks_per_worker: usize) -> Self {
        self.max_tasks_per_worker = max_tasks_per_worker;
        self
    }

    pub async fn start(self, bind_addr: SocketAddr) -> anyhow::Result<UdpServer> {
        let factory = Arc::new(self.factory);
        let max_packet_size = self.max_packet_size;
        let max_tasks_per_worker = self.max_tasks_per_worker;
        let recorder = metrics::recorder();
        let bind_addr = {
            if bind_addr.port() == 0 {
                let socket = socket::UdpSocketConfig::new()
                    .multi_bind()
                    .bind(bind_addr)?;
                socket.local_addr()?
            } else {
                bind_addr
            }
        };

        let key_workers = Key::from_static_name(KEY_UDP_SERVER_WORKERS)
            .with_extra_labels(vec![Label::new("addr", bind_addr.to_string())]);
        let g_workers = recorder.register_gauge(&key_workers);
        let mut arbiters = Vec::new();
        let (start_tx, mut start_rx) = tokio::sync::mpsc::channel(self.workers);

        for worker_idx in 0..self.workers {
            let g_workers = g_workers.clone();
            let socket = socket::UdpSocketConfig::new()
                .multi_bind()
                .min_recv_buffer(4 * 1024 * 1024)
                .recv_err()
                .bind(bind_addr)?;
            let factory = factory.clone();
            let start_tx = start_tx.clone();

            let arbiter = {
                Arbiter::with_tokio_rt(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .max_blocking_threads(10)
                        .build()
                        .unwrap()
                })
            };

            let _h = arbiter.spawn(async move {
                let h = tokio::task::spawn_local(async move {
                    let socket = Rc::new(socket);
                    let worker = match factory.new_worker(socket.clone()) {
                        Ok(worker) => worker,
                        Err(e) => {
                            log::error!("failed to start worker {worker_idx}");
                            start_tx.send(Some(e)).await?;
                            anyhow::bail!("failed to start worker");
                        }
                    };
                    let max_packet_size = max_packet_size;
                    let mut buf = BytesMut::with_capacity(max_packet_size * 4);
                    let ws = Arc::new(tokio::sync::Semaphore::new(max_tasks_per_worker));
                    log::info!("worker {} started on {:?}", worker_idx, bind_addr);
                    let _g = InstanceCountGuard::new(g_workers);
                    start_tx.send(None).await?;
                    loop {
                        let g = ws.clone().acquire_owned().await?;
                        buf.reserve(max_packet_size);
                        let (src_addr, pt) = socket.recv_any(&mut buf).await?;
                        /*let src_addr= socket.recv_from(&mut buf).await?;
                        let pt = PacketType::Data;*/
                        let packet = buf.split();
                        let task = worker.handle(packet, src_addr, pt);
                        tokio::task::spawn_local(async move {
                            if let Err(e) = task.await {
                                log::error!("[{worker_idx}][{src_addr}] invalid request: {:?}", e);
                            }
                            drop(g);
                        });
                    }
                });

                let err = match h.await {
                    Err(e) => e.into(),
                    Ok(Err(e)) => e,
                    Ok(Ok(())) => anyhow::anyhow!("stop"),
                };
                log::error!("worker {} crashed: {:?}", worker_idx, err);
            });
            arbiters.push(arbiter);
        }

        for _ in 0..self.workers {
            if let Some(Some(e)) = start_rx.recv().await {
                arbiters.into_iter().for_each(|a| {
                    a.stop();
                });
                return Err(e);
            }
        }

        Ok(UdpServer {
            arbiters,
            bind_addr,
        })
    }
}

impl UdpServer {
    pub fn stop(self) {
        for arbiter in self.arbiters {
            arbiter.stop();
        }
    }

    pub(crate) fn stop_internal(&self) {
        for arbiter in &self.arbiters {
            arbiter.stop();
        }
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.bind_addr
    }
}

impl<Out: Worker, F: Fn(Rc<UdpSocket>) -> anyhow::Result<Out>> WorkerFactory for F {
    type Worker = Out;

    fn new_worker(&self, reply: Rc<UdpSocket>) -> anyhow::Result<Self::Worker> {
        self(reply)
    }
}

struct FnWrap<F>(F);

impl<Output, F: Fn(BytesMut, SocketAddr, PacketType) -> Output> Worker for FnWrap<F>
where
    F::Output: Future<Output = anyhow::Result<()>>,
{
    type Fut = F::Output;

    fn handle(&self, request: BytesMut, src: SocketAddr, pt: PacketType) -> Self::Fut {
        self.0(request, src, pt)
    }
}

pub fn worker_fn<OutputFut, F>(f: F) -> anyhow::Result<impl Worker>
where
    OutputFut: Future<Output = anyhow::Result<()>>,
    F: Fn(BytesMut, SocketAddr) -> anyhow::Result<OutputFut>,
{
    Ok(FnWrap(move |request, src, pt| {
        if matches!(pt, PacketType::Data) {
            match f(request, src) {
                Ok(fut) => fut.left_future(),
                Err(e) => future::ready(Err(e)).right_future(),
            }
        } else {
            log::debug!("[{:?}] got {:?}", src, pt);
            future::ready(Ok(())).right_future()
        }
    }))
}

pub fn worker_err_fn<OutputFut, F>(f: F) -> anyhow::Result<impl Worker>
where
    OutputFut: Future<Output = anyhow::Result<()>>,
    F: Fn(PacketType, BytesMut, SocketAddr) -> anyhow::Result<OutputFut>,
{
    Ok(FnWrap(move |request, src, pt| match f(pt, request, src) {
        Ok(fut) => fut.left_future(),
        Err(e) => future::ready(Err(e)).right_future(),
    }))
}

pub fn register_metrics() {
    let recorder = metrics::recorder();

    recorder.describe_gauge(
        KEY_UDP_SERVER_WORKERS.into(),
        Some(Unit::Count),
        "number of server workers".into(),
    );
}
