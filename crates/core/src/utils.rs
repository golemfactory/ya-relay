use anyhow::Context;
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
