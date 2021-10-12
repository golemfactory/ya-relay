## Running server

`cargo run -p ya-net-server -- -a udp://127.0.0.1:8888`

## Running client

Initialize session with server:

`cargo run --example client -- -a udp://127.0.0.1:8888 init`

Query information about Node:

`cargo run --example client -- -a udp://127.0.0.1:8888 find-node -n 0x95369fc6fd02afeca110b9c32a21fb8ad899ee0a`

Ping node:

`cargo run --example client -- -a udp://127.0.0.1:8888 ping`

## Running test suite

`RUST_LOG=info cargo test -- --nocapture`
