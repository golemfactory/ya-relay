from python_on_whales import docker
from utils import set_netem, Cluster, Client, Server
import time
import pytest
import logging
import numpy as np
from sklearn.linear_model import LinearRegression

LOGGER = logging.getLogger(__name__)

def ping_nodes(client0, clients):
    ping_times = []
    for client in clients:
        ping_response = client0.ping(client.node_id)
        LOGGER.info(f"Ping duration {ping_response['duration']}ms")
        ping_times.append(ping_response["duration"])
    return ping_times

def calc(ping_times):
    n = len(ping_times)
    mean = sum(ping_times) / n
    deviations = [(t - mean) ** 2 for t in ping_times]
    variance = sum(deviations) / n
    min_ping = min(ping_times)
    max_ping = max(ping_times)

    x = np.array(range(len(ping_times)))
    y = np.array(ping_times)
    x = x.reshape(-1,1)
    regression_model = LinearRegression()

    regression_model.fit(x, y)

    a = regression_model.coef_[0]
    b = regression_model.intercept_
    return (a, b, min_ping, max_ping, mean, variance)

def test_many_pings(compose_up):
    cluster: Cluster = compose_up(10)

    server: Server = cluster.servers()[0]
    client0 = cluster.clients()[0]
    clients = cluster.clients()[1:]


    ping_times_init = ping_nodes(client0, clients)

    ping_times_1 = ping_nodes(client0, clients)
    (a, b, min_ping, max_ping, mean, variance) = calc(ping_times_1)
    LOGGER.info(f"Mean ping time: {mean}ms")
    LOGGER.info(f"Min ping time: {min_ping}ms")
    LOGGER.info(f"Max ping time: {max_ping}ms")
    LOGGER.info(f"Variance: {variance}ms")
    LOGGER.info(f"Linear regression: a={a}, b={b}")

    assert abs(a) < 0.15  # TODO still assuming ping times might be increasing with time

    for client in clients:
        find_response = client0.find(client.node_id)
        # identities = find_response["node"]["identities"]

    ping_times_2 = ping_nodes(client0, clients)
    (a, b, min_ping, max_ping, mean, variance) = calc(ping_times_2)
    LOGGER.info(f"Mean ping time: {mean}ms")
    LOGGER.info(f"Min ping time: {min_ping}ms")
    LOGGER.info(f"Max ping time: {max_ping}ms")
    LOGGER.info(f"Variance: {variance}ms")
    LOGGER.info(f"Linear regression: a={a}, b={b}")

    assert abs(a) < 0.15  # TODO still assuming ping times might be increasing with time

    LOGGER.info(f"Done")
