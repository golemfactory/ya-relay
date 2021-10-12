use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

const FLAG_OPEN: u8 = 0x01;
const FLAG_AWAKENED: u8 = 0x02;

pub fn queue<T>() -> (Sender<T>, Receiver<T>) {
    let inner: Arc<Mutex<Inner<T>>> = Default::default();
    let tx = Sender {
        inner: inner.clone(),
    };
    let rx = Receiver { inner };
    (tx, rx)
}

pub fn queue_with_capacity<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Mutex::new(Inner {
        queue: VecDeque::with_capacity(capacity),
        ..Default::default()
    }));
    let tx = Sender {
        inner: inner.clone(),
    };
    let rx = Receiver { inner };
    (tx, rx)
}

#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Queue closed")]
    Closed,
}

pub struct Receiver<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

#[derive(Clone)]
pub struct Sender<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

struct Inner<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
    flags: u8,
}

impl<T> Receiver<T> {
    pub fn is_closed(&self) -> bool {
        is_closed(&self.inner)
    }

    pub fn close(&mut self) {
        close(&mut self.inner);
    }
}

impl<T> futures::Stream for Receiver<T> {
    type Item = T;

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
            Some(item) => {
                if inner.queue.is_empty() {
                    inner.flags &= !FLAG_AWAKENED;
                } else {
                    cx.waker().wake_by_ref();
                }
                Poll::Ready(Some(item))
            }
            None => {
                inner.flags &= !FLAG_AWAKENED;
                Poll::Pending
            }
        }
    }
}

impl<T> Sender<T> {
    pub fn send(&mut self, item: T) -> Result<(), Error> {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.push_back(item);

        if inner.flags & FLAG_OPEN != FLAG_OPEN {
            return Err(Error::Closed);
        } else if inner.flags & FLAG_AWAKENED != FLAG_AWAKENED {
            if let Some(waker) = inner.waker.as_ref() {
                waker.wake_by_ref();
                inner.flags |= FLAG_AWAKENED;
            }
        }

        Ok(())
    }

    pub fn send_all(&mut self, items: impl IntoIterator<Item = T>) -> Result<(), Error> {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.extend(items);

        if inner.flags & FLAG_OPEN != FLAG_OPEN {
            return Err(Error::Closed);
        } else if inner.flags & FLAG_AWAKENED != FLAG_AWAKENED {
            if let Some(waker) = inner.waker.as_ref() {
                waker.wake_by_ref();
                inner.flags |= FLAG_AWAKENED;
            }
        }

        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        is_closed(&self.inner)
    }

    pub fn close(&mut self) {
        close(&mut self.inner);
    }
}

impl<T> Default for Inner<T> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            waker: None,
            flags: FLAG_OPEN,
        }
    }
}

#[inline(always)]
fn is_closed<T>(inner: &Arc<Mutex<Inner<T>>>) -> bool {
    let inner = inner.lock().unwrap();
    inner.flags & FLAG_OPEN != FLAG_OPEN
}

#[inline(always)]
fn close<T>(inner: &mut Arc<Mutex<Inner<T>>>) {
    let mut inner = inner.lock().unwrap();
    inner.flags &= !FLAG_OPEN;
    inner.queue.clear();
}
