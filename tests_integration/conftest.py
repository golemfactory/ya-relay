import pytest
from python_on_whales import DockerClient
from utils import Cluster, Scales

default_build_args = {
    # general settings
    "SERVER_LATENCY": "20ms",
    "CLIENT_LATENCY": "5ms",
    "RUST_LOG": "info",
    # public client
    "PUBLIC_BIND_IP": "",
    "PUBLIC_BIND_IP_PATTERN": "",
    "PUBLIC_BIND_PROTO": "tcp",
    # alice client
    "ALICE_BIND_IP": "",
    "ALICE_BIND_IP_PATTERN": "",
    "ALICE_BIND_PROTO": "tcp",
    # bob client
    "BOB_BIND_IP": "",
    "BOB_BIND_IP_PATTERN": "",
    "BOB_BIND_PROTO": "tcp",
    "SESSION_EXPIRATION": "",
}


@pytest.fixture(scope="session", autouse=True)
def base_build():
    print("Base build")
    docker = DockerClient()
    docker.build("../test_env", file="../test_env/Dockerfile.base", tags="tests_base")


@pytest.fixture(scope="package")
def compose_build() -> DockerClient:
    print("Package level Compose Build")
    return __compose_build(default_build_args)


def __compose_build(build_args) -> DockerClient:
    print("Docker Compose Build")
    __write_dot_env(build_args)
    docker = DockerClient(compose_files=["docker-compose.yml"])
    docker.compose.build(build_args=build_args)
    return docker


def __write_dot_env(properties):
    with open(".env", "w") as f:
        for key, value in properties.items():
            f.write("%s=%s\n" % (key, value))


# Factory fixture (returns function)
# Requires `compose_build` fixture
@pytest.fixture(scope="function")
def compose_up(compose_build: DockerClient):
    def _compose_up(
        public_clients: int, alice_clients: int = 0, bob_clients: int = 0, servers: int = 1, build_args=None
    ) -> Cluster:
        scales = Scales(
            public_client=public_clients, alice_client=alice_clients, bob_client=bob_clients, relay_server=servers
        )
        _compose_build = compose_build
        if build_args is not None:
            build_args = default_build_args.copy() | build_args
            _compose_build = __compose_build(build_args)
        cluster = Cluster(_compose_build)
        cluster.start(scales)
        return cluster

    return _compose_up


@pytest.fixture(scope="function", autouse=True)
def compose_down(compose_build: DockerClient):
    yield  # wait for test to finish
    print("Docker Compose Down")
    compose_build.compose.down()
