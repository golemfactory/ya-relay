use anyhow::Context;
use futures::future::{AbortHandle, Abortable};
use std::future::Future;

pub use url::Url;

pub fn parse_udp_url(url: &Url) -> anyhow::Result<String> {
    let host = url.host_str().context("Needs host for NET URL")?;
    let port = url.port().unwrap_or(*crate::DEFAULT_NET_PORT);

    Ok(format!("{}:{}", host, port))
}

// Extract typed data from environment variable
pub fn typed_from_env<T: std::str::FromStr + Copy>(env_key: &str, def_value: T) -> T {
    std::env::var(env_key)
        .map(|s| s.parse::<T>().unwrap_or(def_value))
        .unwrap_or(def_value)
}

pub fn spawn_local_abortable<F>(future: F) -> AbortHandle
where
    F: Future + 'static,
    F::Output: 'static,
{
    let (abort_handle, abort_registration) = AbortHandle::new_pair();

    tokio::task::spawn_local(Abortable::new(future, abort_registration));
    abort_handle
}

pub trait ResultExt<T, E>: Sized {
    fn on_error<F: FnOnce(&E)>(self, op: F) -> Result<T, E>;
    fn on_done<F: FnOnce(&Result<T, E>)>(self, op: F) -> Result<T, E>;
}

impl<T, E> ResultExt<T, E> for Result<T, E> {
    fn on_error<F: FnOnce(&E)>(self, op: F) -> Result<T, E> {
        if let Err(e) = &self {
            op(&e);
        }
        self
    }

    fn on_done<F: FnOnce(&Result<T, E>)>(self, op: F) -> Result<T, E> {
        op(&self);
        self
    }
}
