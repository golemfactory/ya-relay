## Running server

`RUST_LOG=info cargo run -p ya-net-server -- -a udp://127.0.0.1:8888`

## Running client

`RUST_LOG=info cargo run --example client -- -a udp://127.0.0.1:8888`