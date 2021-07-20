## Running server

`RUST_LOG=info cargo run -p ya-net-server -- -a udp://127.0.0.1:8888`

## Running client

Initialize session with server:

`RUST_LOG=info cargo run --example client -- -a udp://127.0.0.1:8888 init -n 0x95369fc6fd02afeca110b9c32a21fb8ad899ee0a`

Query information about Node:

`RUST_LOG=info cargo run --example client -- -a udp://127.0.0.1:8888 find-node --session-id 921A24E03B9B44F39E417662AFC1F34F -n 0x95369fc6fd02afeca110b9c32a21fb8ad899ee0a`

Ping node:

`RUST_LOG=info cargo run --example client -- -a udp://127.0.0.1:8888 ping --session-id 921A24E03B9B44F39E417662AFC1F34F`

## Running test suite

`RUST_LOG=info cargo test -- --nocapture`
