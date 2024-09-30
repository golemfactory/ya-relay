import { html, LitElement } from "lit";
import { MetricsService } from "./metrics.js";

import "./elements/info-box.js";
import "./elements/nav-bar.js";

const HEX_CHARS = "0123456789abcdef".split("");

async function* fetch_nodes(prefix = "") {
  async function nodesForPrefix(prefix) {
    let r = await fetch(`/nodes/${prefix === "" ? "all" : prefix}`);
    let node_map = await r.json();
    const nodes = Object.keys(node_map).map((node_id) => {
      return {
        node_id: node_id,
        sessions: node_map[node_id],
      };
    });
    nodes.sort((n1, n2) => n2.node_id < n1.node_id ? 1 : -1)
    return nodes;
  }

  const nodes = await nodesForPrefix(prefix);
  if (nodes.length < 50) {
    for (const node of nodes) {
      yield node;
    }
  } else {
    for (const hex of HEX_CHARS) {
      for await (const node of fetch_nodes(`${prefix}${hex}`)) {
        yield node;
      }
    }
  }
}

class AppElement extends LitElement {
  static properties = {
    _metrics: { state: true },
    _nodes: { state: true },
    _pings: { state: true },
    _selected_session: { state: true },
  };

  constructor() {
    super();
    this._metrics = { nodes: "N/A", sessions: "N/A" };
    this._nodes = [];
    this._next_nodes = null;
    this._pings = [];
    this._selected_session = null;
  }

  createRenderRoot() {
    return this;
  }

  connectedCallback() {
    super.connectedCallback();
    this.refreshStatus();
    this._next_nodes = fetch_nodes();
    this.pullNodes();
  }

  async pullNodes() {
    const nodes = Array.from(this._nodes);
    for (let i = 0; i < 10; ++i) {
      const { done, value } = await this._next_nodes.next();
      if (done) {
        this._next_nodes = null;
        break;
      }
      nodes.push(value);
    }
    this._nodes = nodes;
  }

  async refreshStatus() {
    let r = await fetch("/status");
    let { nodes, sessions } = await r.json();
    this._metrics = { nodes, sessions };
  }

  async refreshNodes() {
    this._nodes = [];
    this._next_nodes = fetch_nodes();
    await this.pullNodes();
  }

  renderSessions({ sessionId, peer, seen, addrStatus }) {
    const select_session = async (e) => {
      e.stopPropagation();
      this._selected_session = { sessionId, peer, seen, addrStatus };
    };

    return html`
            <td><p>
                <a href="#" @click=${select_session}>${sessionId}</a>
            </p></td>
            <td>${peer}</td>
            <td>${seen}</td>
            <td>${addrStatus}</td>        
        `;
  }

  renderTable() {

    const parseSeen = (seen) => seen.endsWith("ms") ? parseFloat(seen.substring(0, seen.length-2))*0.001 : parseFloat(seen.substring(0, seen.length-1))

    const formatSeen = (seen) => `${Math.round(seen, 1)} s ago`

    const lastResponse = (sessions) => sessions.length == 0 ? 'N/A' : formatSeen(Math.max(... sessions.map((session) => parseSeen(session.seen))));

    const peer = (sessions) => sessions.length == 0 ? "N/A" : sessions[0].peer;

    const addrStatus = (sessions) => sessions.length == 0 ? "" : sessions.reduce((v, s) => v || s.addrStatus.startsWith('valid('), false) ?
        html`<div class="align-middle"><span class="badge badge-success">✓</span></div>` : html``;


    return html`
      <table class="table table-hover">
                    <thead>
                    <tr>
                        <th>Node Id</th>
                        <th>Last Response</th>
                        <th>Session Count</th>
                        <th>Peer</th>
                        <th>Public IP</th>
                    </tr>
                    </thead>
                    <tbody>
                    ${this._nodes.map(({ node_id, sessions }) =>
                        html`<tr>
                                    <td>${node_id} <a href="#" class="btn btn-link">➡️<a/></td>
                                    <td>${lastResponse(sessions)}</td>
                                    <td>${sessions.length}</td>
                                    <td>${peer(sessions)}</td>
                                    <td>${addrStatus(sessions)}</td>
                        </tr>`)}    
                    </tbody>
                </table>
                ${
      this._next_nodes
        ? html`<button class="btn btn-primary mb-2" @click="${this.pullNodes}">More</button>`
        : null
    }`;
  }

  render() {
    return html`
            <header>
                <nav-bar></nav-bar>
                <div class="container-fluid mt-4">
                    <div class="row">
                        <div class="col-md-4">
                            <div class="card text-white bg-success mb-3">
                                <div class="card-header">Connected Sessions</div>
                                <div class="card-body">
                                    <h5 class="card-title">${this._metrics.sessions} Active</h5>
                                    <p class="card-text">Monitor active sessions and their statuses.</p>
                                </div>
                            </div>
                        </div>
                        <div class="col-md-4">
                            <div class="card text-white bg-info mb-3">
                                <div class="card-header">Nodes Status</div>
                                <div class="card-body">
                                    <h5 class="card-title">${this._metrics.nodes} Nodes</h5>
                                    <p class="card-text">Details on different nodes and their responsiveness.</p>
                                </div>
                            </div>
                        </div>
                        <div class="col-md-4">
                            <div class="card text-white bg-warning mb-3">
                                <div class="card-header">Low Response Nodes</div>
                                <div class="card-body">
                                    <h5 class="card-title">15 Slow</h5>
                                    <p class="card-text">Nodes last seen > 1 minute ago.</p>
                                </div>
                            </div>
                        </div>
                    </div>
                </div>
            </header>
            <main class="container-fluid mt-3">
                <h2>Connected Nodes</h2>            
                <div class="row">
                    ${
      this._pings.map(({ sessionId, status }) =>
        html`<p>${sessionId} = ${status}</p>`
      )
    }
                </div>               
                <div class="row">
                    <div class="col-sm m-1">
                        <button class="btn btn-lg btn-secondary" @click="${this.refreshNodes}">Refresh</button>
                    </div>
                </div>                
                ${
      this._selected_session ? this.renderSession() : this.renderTable()
    }
            </main>
        `;
  }

  renderSession() {
    const { sessionId, peer, seen, addrStatus } = this._selected_session;

    const on_done = (e) => {
      e.preventDefault();
      this._selected_session = null;
    };

    const on_check_click = async (e) => {
      e.stopPropagation();
      this._pings = [{ sessionId, status: "W" }];
      const r = await fetch(`/sessions/${sessionId}/check`, {
        method: "POST",
      });
      let b = await r.json();
      this._pings = [{ sessionId, status: b }];
    };

    const on_delete_click = async (e) => {
      e.stopPropagation();
      console.log(`delete ${sessionId}`);
      const r = await fetch(`/sessions/${sessionId}`, {
        method: "DELETE",
      });
      this.refreshNodes();
    };

    return html`<div class="row">
            <div class="col-12">
                <p>${sessionId}</p>
                <p>${addrStatus}</p>
                <p>${peer}</p>
                <p>${seen} </p>
                <button class="btn btn-success" @click=${on_done}>Done</button>
                <button class="btn btn-secondary" @click=${on_check_click}>Recheck Ip</button>
                <button class="btn btn-danger" @click=${on_delete_click}>Kick</button>
            </div>
        </div>`;
  }
}
window.customElements.define("x-app", AppElement);
