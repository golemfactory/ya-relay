import {LitElement, html} from 'lit';
import {MetricsService} from "./metrics.js";

import './elements/info-box.js';

const HAX_CHARS = "0123456789abcdef".split('');
async function* fetch_nodes(prefix = '') {
    async function nodesForPrefix(prefix) {
        let r = await fetch(`/nodes/${prefix === '' ? 'all' : prefix}`);;
        let nodes = await r.json();
        return nodes;
    }

    const nodes = await nodesForPrefix(prefix);
    if (nodes.length < 50) {
        for (const node of nodes) {
            yield node
        }
    }
    else {
        for (const hex of HAX_CHARS) {
            for await (const node of fetch_nodes(`${prefix}${hex}`)) {
                yield node
            }
        }
    }
}

class AppElement extends LitElement {

    static properties = {
        _metrics: {state: true},
        _nodes: {state: true},
        _pings: {state:true}
    };


    constructor() {
        super();
        this._metrics = {nodes: 'N/A', sessions: 'N/A'};
        this._nodes = [];
        this._pings = [];
    }

    createRenderRoot() {
        return this;
    }

    connectedCallback() {
        super.connectedCallback()
        this.refreshStatus();
        this.refreshNodes();
    }


    async refreshStatus() {
        let r = await fetch("/status");
        let {nodes, sessions} = await r.json();
        this._metrics = {nodes, sessions};
    }

    async refreshNodes() {

        async function nodesForPrefix(prefix) {
            let r = await fetch(`/nodes/${prefix}`);;
            let nodes = await r.json();
            return nodes;
        }


        const nodes = [];
        for (const hd of hex) {
            const np = await nodesForPrefix(hd);
            for (const node_id of Object.keys(np)) {
                const sessions = np[node_id];
                nodes.push({node_id, sessions});
            }
        }
        nodes.sort((a,b) => {
            if (a.node_id < b.node_id) {
                return -1;
            }
            if (a.node_id > b.node_id) {
                return 1;
            }
            return 0;
        });
        this._nodes = nodes;
    }

    renderSessions({sessionId, peer, seen, addrStatus}) {
        const on_delete_click = async (e) => {
            e.stopPropagation();
            console.log(`delete ${sessionId}`);
            const r = await fetch(`/sessions/${sessionId}`, {
                method: "DELETE"
            });
            this.refreshNodes();
        }

        const on_check_click = async(e) => {
            e.stopPropagation();
            this._pings = [{sessionId, status: 'W'}];
            const r = await fetch(`/sessions/${sessionId}/check`, {
                method: "POST"
            });
            let b = await r.json();
            this._pings = [{sessionId, status: b}];
        };

        return html`
            <td><p>
                ${sessionId}
                <a href="#" class="btn btn-sm btn-danger" @click=${on_delete_click}>Delete</a>
                <a href="#" class="btn btn-sm btn-info" @click=${on_check_click}>Recheck</a>
            </p></td>
            <td>${peer}</td>
            <td>${seen}</td>
            <td>${addrStatus}</td>        
        `;
    }

    render() {
        return html`
            <header>
            <nav class="navbar navbar-expand-lg navbar-dark bg-dark">
                <div class="container">
                    <a class="navbar-brand" href="#">Relay</a>
                    <button class="navbar-toggler" type="button" data-toggle="collapse" data-target="#navbarResponsive" aria-controls="navbarResponsive" aria-expanded="false" aria-label="Toggle navigation">
                        <span class="navbar-toggler-icon"></span>
                    </button>
                    <div class="collapse navbar-collapse" id="navbarResponsive">
                        <ul class="navbar-nav ml-auto">
                            <li class="nav-item active">
                                <a class="nav-link" href="#">Home
                                    <span class="sr-only">(current)</span>
                                </a>
                            </li>                            
                        </ul>
                    </div>
                </div>
            </nav>
            </header>
            <main>
            <div class="container">                
                <div class="row">
                    ${this._pings.map(({sessionId, status}) => html`<p>${sessionId} = ${status}</p>`)}
                </div>
                <div class="row">
                    <div class="col-sm m-1">
                        <info-box title="Sessions" value="${this._metrics.sessions}"></info-box>
                    </div>
                    <div class="col-sm m-1">
                        <info-box title="Nodes" value="${this._metrics.nodes}"></info-box>
                    </div>
                </div>
                <div class="row">
                    <div class="col-sm m-1">
                        <button class="btn btn-lg btn-secondary" @click="${this.refreshNodes}">Refresh</button>
                    </div>
                </div>
                <div class="row">
                    <div class="col-12">
                        <table class="table table-striped m-2">
                            <thead>                                
                                <tr>
                                    <th>nodeId</th>
                                    <th>sessionId</th>
                                    <th>peer</th>
                                    <th>seen</th>
                                    <th>addrStatus</th>
                                </tr>
                            </thead>
                            <tbody>
                                ${this._nodes.map(({node_id, sessions}) => html`
                                    <tr>
                                        <td rowspan="${sessions.length}">${node_id}</td>
                                        ${sessions[0] == null ? "N/A" : this.renderSessions(sessions[0])}
                                    </tr>
                                    ${sessions.splice(1).map((session) => html`<tr>${session == null ? "N/A" : this.renderSessions(session)}</tr>`)}
                                `)}
                            </tbody>
                        </table>
                               
                    </div>
                </div>
            </div>
            </main>
        `;
    }
}
window.customElements.define('x-app', AppElement);
