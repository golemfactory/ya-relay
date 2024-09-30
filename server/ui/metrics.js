import {
  html,
  LitElement,
} from "https://unpkg.com/lit-element@3/lit-element.js?module";

export class MetricsService {
  async refresh() {
    const data = await fetch("/metrics");
    const lines = (await data.text()).split("\n");
    let kv = new Map();
    for (const line of lines) {
      if (line.startsWith("#") || line.trim() === "") {
        continue;
      }
      const [k, v] = line.split(/\s+/);
      kv.set(k, v);
    }
    return kv;
  }
}
