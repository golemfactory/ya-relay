import {LitElement, UpdatingElement, html} from 'lit';

class InfoBoxElement extends LitElement {

    static properties = {
        title: {type: String},
        value: {type: String}
    };

    createRenderRoot() {
        return this;
    }

    render() {
        return html`
            <div class="card">
                <div class="card-header">${this.title}</div>
                <div class="card-body">                    
                    <p class="card-text">
                        ${this.value}
                        <slot></slot>
                    </p>
                </div>
            </div>
        `;
    }


}

window.customElements.define('info-box', InfoBoxElement);