//! General wraper for calling non-send objects from other threads.

use futures::future::LocalBoxFuture;
use futures::prelude::*;
use std::marker::PhantomData;
use tokio::sync::{mpsc, oneshot};

pub struct SendWrap<T> {
    tx: mpsc::Sender<Box<dyn Runnable<T> + Send + 'static>>,
}

trait Runnable<T> {
    fn exec(self: Box<Self>, item: &T) -> future::LocalBoxFuture<()>;
}

pub trait AsyncFnOnce<Arg> {
    type Output;
    type Fut : Future<Output=Self::Output>;

    fn call(self, arg : Arg) -> Self::Fut;

}

impl<Arg, RF : Future, F : FnOnce(Arg)-> RF> AsyncFnOnce<Arg> for F {
    type Output = RF::Output;
    type Fut = RF;

    fn call(self, arg: Arg) -> Self::Fut {
        self(arg)
    }
}


struct Call<T, F : AsyncFnOnce<T>>
{
    reply: oneshot::Sender<F::Output>,
    inner: F,
    _marker: PhantomData<fn() -> T>,
}

impl<T, F : AsyncFnOnce<T>> Runnable<T> for Call<T, F>
{
    fn exec<'a>(self: Box<Self>, item: &'a T) -> LocalBoxFuture<'a, ()> {
        let inner = self.inner;
        let reply = self.reply;
        async move {
            let result = inner.call(item).await;
            let _ = reply.send(result);
        }
        .boxed_local()
    }
}

impl<T : 'static> SendWrap<T> {
    pub async fn run_async<F : AsyncFnOnce<T> + Send + 'static>(
        &mut self,
        f: F,
    ) -> Result<F::Output, tokio::sync::oneshot::error::RecvError> where F::Output : Send

    {
        let (rx, tx) = oneshot::channel();
        let _ = self.tx.send(Box::new(Call {
            inner: f,
            reply: rx,
            _marker: PhantomData,
        }));
        tx.await
    }
}

pub fn wrap<T: 'static>(item: T) -> SendWrap<T> {
    let (tx, mut rx) = mpsc::channel::<Box<dyn Runnable<T> + Send + 'static>>(1);

    let h = tokio::task::spawn_local(async move {
        while let Some(runnable) = rx.recv().await {
            runnable.exec(&item).await
        }
    });

    SendWrap { tx }
}
