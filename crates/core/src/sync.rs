use std::ops::Not;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::oneshot;
use futures::future::{BoxFuture, Shared};
use futures::FutureExt;

#[derive(Clone, Default)]
pub struct Actuator {
    enabled: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}

impl Actuator {
    #[inline]
    pub fn next(&self) -> Option<BoxFuture<'static, ()>> {
        self.is_enabled()
            .then(|| self.state.lock().unwrap().future())
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn enable(&self) {
        self.is_enabled().not().then(|| self.reset(true));
    }

    #[inline]
    pub fn disable(&self) {
        self.is_enabled().then(|| self.reset(false));
    }

    fn reset(&self, to: bool) {
        let mut state = self.state.lock().unwrap();
        *state = State::default();
        self.enabled.store(to, Ordering::SeqCst);
    }
}

struct State {
    tx: Option<oneshot::Sender<()>>,
    rx: Shared<BoxFuture<'static, ()>>,
}

impl State {
    #[inline]
    fn future(&self) -> BoxFuture<'static, ()> {
        self.rx.clone().boxed()
    }
}

impl Default for State {
    fn default() -> Self {
        let (tx, rx) = oneshot::channel();
        Self {
            tx: Some(tx),
            rx: rx.then(|_| futures::future::ready(())).boxed().shared(),
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}
