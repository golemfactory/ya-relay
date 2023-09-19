- go to main dir of ya_relay
- build release
  `cargo build --release`
  `cargo build -p ya-relay-client --example http_client --release`
- run:
  docker compose -f test_env/docker-compose.yml up --build client --build relay_server --scale client=2 
- check client ports via `docker ps`
- you can access them via `curl localhost:8080/ping/0x1234...`
