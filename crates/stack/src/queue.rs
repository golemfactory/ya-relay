use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::FutureExt;

use crate::error::Error;

pub type Result<T> = std::result::Result<T, Error>;
pub type ProcessedFuture<'a> = BoxFuture<'a, Result<()>>;
type ItemSender<T> = oneshot::Sender<Result<T>>;

const FLAG_OPEN: u8 = 0x01;
const FLAG_AWAKENED: u8 = 0x02;

pub struct Queue<T> {
    sender: Sender<T>,
    receiver: Option<Receiver<T>>,
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        let inner: Arc<Mutex<SenderInner<T>>> = Default::default();

        Queue {
            sender: Sender {
                inner: inner.clone(),
            },
            receiver: Some(Receiver { inner }),
        }
    }
}

impl<T> Queue<T> {
    pub fn sender(&mut self) -> Sender<T> {
        self.sender.clone()
    }

    pub fn receiver(&mut self) -> Option<Receiver<T>> {
        self.receiver.take()
    }

    #[allow(unused)]
    pub fn is_closed(&self) -> bool {
        let inner = self.sender.inner.lock().unwrap();
        inner.flags & FLAG_OPEN != FLAG_OPEN
    }

    #[allow(unused)]
    pub fn close(&mut self) {
        let mut inner = self.sender.inner.lock().unwrap();
        inner.flags &= !FLAG_OPEN;

        std::mem::take(&mut inner.queue)
            .into_iter()
            .for_each(|(_, tx)| {
                let _ = tx.send(Err(Error::QueueClosed));
            });
    }
}

pub struct Receiver<T> {
    inner: Arc<Mutex<SenderInner<T>>>,
}

impl<T> futures::Stream for Receiver<T> {
    type Item = (T, ItemSender<()>);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.flags & FLAG_OPEN != FLAG_OPEN {
            return Poll::Ready(None);
        }

        match inner.waker.as_ref() {
            Some(waker) => {
                let cx_waker = cx.waker();
                if !cx_waker.will_wake(waker) {
                    inner.waker.replace(cx_waker.clone());
                    inner.flags &= !FLAG_AWAKENED;
                }
            }
            None => {
                inner.waker.replace(cx.waker().clone());
                inner.flags &= !FLAG_AWAKENED;
            }
        }

        match inner.queue.pop_front() {
            Some((item, tx)) => {
                if inner.queue.is_empty() {
                    inner.flags &= !FLAG_AWAKENED;
                } else {
                    cx.waker().wake_by_ref();
                }
                Poll::Ready(Some((item, tx)))
            }
            None => {
                inner.flags &= !FLAG_AWAKENED;
                Poll::Pending
            }
        }
    }
}

pub struct Sender<T> {
    inner: Arc<Mutex<SenderInner<T>>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Sender<T> {
    pub fn send<'a>(&mut self, item: T) -> Result<ProcessedFuture<'a>> {
        let (tx, rx) = oneshot::channel();

        let mut inner = self.inner.lock().unwrap();
        inner.queue.push_back((item, tx));

        if inner.flags & FLAG_OPEN != FLAG_OPEN {
            return Err(Error::QueueClosed);
        } else if inner.flags & FLAG_AWAKENED != FLAG_AWAKENED {
            if let Some(waker) = inner.waker.as_ref() {
                waker.wake_by_ref();
                inner.flags |= FLAG_AWAKENED;
            }
        }

        Ok(rx
            .then(|r| async move {
                match r {
                    Ok(result) => result,
                    Err(_) => Err(Error::QueueClosed),
                }
            })
            .boxed())
    }
}

struct SenderInner<T> {
    queue: VecDeque<(T, ItemSender<()>)>,
    waker: Option<Waker>,
    flags: u8,
}

impl<T> Default for SenderInner<T> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            waker: None,
            flags: FLAG_OPEN,
        }
    }
}
