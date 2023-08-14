from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import time
import pytest
import logging

LOGGER = logging.getLogger(__name__)


def linreg(X, Y):
    """
    return a,b in solution to y = ax + b such that root mean square distance between trend line and original points is minimized
    """
    N = len(X)
    Sx = Sy = Sxx = Syy = Sxy = 0.0
    for x, y in zip(X, Y):
        Sx = Sx + x
        Sy = Sy + y
        Sxx = Sxx + x * x
        Syy = Syy + y * y
        Sxy = Sxy + x * y
    det = Sxx * N - Sx * Sx
    return (Sxy * N - Sy * Sx) / det, (Sxx * Sy - Sx * Sxy) / det


def test_many_pings(compose_up):
    cluster: Cluster = compose_up(10)

    server: Server = cluster.servers()[0]
    client0 = cluster.clients()[0]
    clients = cluster.clients()[1:]

    for client in clients:
        LOGGER.info(f"Find client {client.node_id}")

        find_response = client0.find(client.node_id)
        identities = find_response["node"]["identities"]
        LOGGER.info(f"{identities}")

    LOGGER.info(f"Ping clients")
    ping_times = []

    for client in clients:
        ping_response = client0.ping(client.node_id)
        LOGGER.info(f"Ping duration {ping_response['duration']}ms")
        ping_times.append(ping_response["duration"])

    n = len(ping_times)
    mean = sum(ping_times) / n
    deviations = [(t - mean) ** 2 for t in ping_times]
    variance = sum(deviations) / n

    LOGGER.info(f"Mean ping time: {mean}ms")
    LOGGER.info(f"Min ping time: {min(ping_times)}ms")
    LOGGER.info(f"Max ping time: {max(ping_times)}ms")
    LOGGER.info(f"Variance: {variance}ms")

    a, b = linreg(range(len(ping_times)), ping_times)  # your x,y are switched from standard notation
    LOGGER.info(f"Linear regression: a={a}, b={b}")

    assert a > -0.1 and a < 0.1  # TODO still assuming ping times might be increasing with time

    LOGGER.info(f"Done")
