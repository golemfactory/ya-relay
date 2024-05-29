## Running server

`cargo run -p ya-relay-server -- -a udp://127.0.0.1:7464`

## Running client

Initialize session with server:

`cargo run -p ya-relay-client --example client -- -a udp://127.0.0.1:7464 init`

Query information about Node:

`cargo run -p ya-relay-client --example client -- -a udp://127.0.0.1:7464 find-node -n 0x95369fc6fd02afeca110b9c32a21fb8ad899ee0a`

Ping node:

`cargo run -p ya-relay-client --example client -- -a udp://127.0.0.1:7464 ping`

## Running test suite

`RUST_LOG=info cargo test -- --nocapture`

## Running examples

### Relay forwarding test

`cargo run -p ya-relay-client --release --example perf relay-traffic --connections 6000 --requestors 3 --scenario-file client/examples/resources/fwds.csv`

To run test locally we need to ensure we have relayed connections with other
Nodes. The best way to do this is to modify `ya-relay-server` so it always
returns no public endpoints for connected Node.

# UI
