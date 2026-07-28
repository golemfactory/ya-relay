# Configuration parameters

## Packet listening

- `--address`, `-a`, `NET_ADDRESS`. **required**, For example udp://0.0.0.0:7464
  accepts udp packets on port 7564.
- `--workers`, `WORKERS`. defines how many sockets with seperate worker threads
  will be created

## Monitoring

- `--metrics`, `RELAY_METRICS`.

## Session Management

### Creation

- `--difficulty`, `DIFFICULTY`. default 16.
- `--salt`, `SALT`. adding static SALT allows you to restart without breaking
  the negotiations that have already started
- `--min-client-version`, `RELAY_MIN_CLIENT_VERSION`. minimum accepted
  `ya-relay-client` SemVer sent in the initial session packet; default `0.7.0`.
  Missing, malformed, prerelease, and older versions are rejected.

### Ip Check

Public IP address validation algorithm.

- `ip-check-retry-after`, `IP_CHECK_RETRY_AFTER`. default 300ms. time after
  which the ping is repeated.
- `ip-check-timeout`, `IP_CHECK_TIMEOUT`. default 1500ms. The time after which a
  node is considered to have no public IP address.
- `ip-check-cache-time`, `IP_CHECK_CACHE_TIME`. default 1m. The time after which
  the verification result is considered negative is kept in the cache.
