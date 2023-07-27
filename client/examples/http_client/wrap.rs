//! General wraper for calling non-send objects from other threads.

use futures::future::LocalBoxFuture;
use futures::prelude::*;
use std::marker::PhantomData;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

#[derive(Error, Debug)]
pub enum Error {
    #[error("wrapped object closed")]
    SendFailed {},
    #[error("{0}")]
    RecvFailed(#[from] oneshot::error::RecvError),
}

pub struct SendWrap<T> {
    tx: mpsc::Sender<Box<dyn Runnable<T> + Send + 'static>>,
}

trait Runnable<T> {
    fn exec<'a>(self: Box<Self>, item: T) -> future::LocalBoxFuture<'a, ()>;
}

pub trait AsyncFnOnce<'a, Arg> {
    type Output: 'static;
    type Fut: Future<Output = Self::Output> + 'a;

    fn call(self, arg: Arg) -> Self::Fut;
}

impl<'a, Arg, RF: Future + 'a, F: FnOnce(Arg) -> RF> AsyncFnOnce<'a, Arg> for F
where
    RF::Output: 'static,
{
    type Output = RF::Output;
    type Fut = RF;

    fn call(self, arg: Arg) -> Self::Fut {
        self(arg)
    }
}

struct Call<T, F, Output> {
    reply: oneshot::Sender<Output>,
    inner: F,
    _marker: PhantomData<fn() -> T>,
}

impl<T: 'static, F: AsyncFnOnce<'static, T> + 'static> Runnable<T> for Call<T, F, F::Output> {
    fn exec<'a>(self: Box<Self>, item: T) -> LocalBoxFuture<'a, ()> {
        let inner = self.inner;
        let reply = self.reply;
        async move {
            let result = inner.call(item).await;
            let _ = reply.send(result);
        }
        .boxed_local()
    }
}
impl<T: 'static> SendWrap<T> {
    /// ## Example
    ///
    /// ```rust
    /// let c = wrap(client);
    ///
    /// tokio::spawn(async move {
    ///     c.run_async(|c| c.sockets()).await
    /// });
    /// ```
    ///
    pub async fn run_async<F: AsyncFnOnce<'static, T> + Send + 'static>(
        &self,
        f: F,
    ) -> Result<F::Output, Error>
    where
        F::Output: Send,
    {
        let (rx, tx) = oneshot::channel();
        let _ = self
            .tx
            .send(Box::new(Call {
                inner: f,
                reply: rx,
                _marker: PhantomData,
            }))
            .await
            .map_err(|_e| Error::SendFailed {});

        tx.await.map_err(|e| Error::RecvFailed(e))
    }
}

pub fn wrap<T: Clone + 'static>(item: T) -> SendWrap<T> {
    let (tx, mut rx) = mpsc::channel::<Box<dyn Runnable<T> + Send + 'static>>(1);

    let _handle = tokio::task::spawn_local(async move {
        while let Some(runnable) = rx.recv().await {
            tokio::task::spawn_local(runnable.exec(item.clone()));
        }
    });

    SendWrap { tx }
}
