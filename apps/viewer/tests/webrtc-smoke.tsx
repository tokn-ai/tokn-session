import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { loadHubCrypto, type DeviceIdentity } from "../src/lib/hubCrypto";
import { EncryptedHubClient } from "../src/lib/hubEncryptedClient";
import type { TransportState } from "../src/lib/types";
import { RemoteConnection } from "../src/components/RemoteConnection";
import "../src/App.css";

interface FixtureConfig { hub_url: string; host_id: string; host_public_key: string; }

function Smoke() {
  const [transport, setTransport] = useState<TransportState>({ kind: "relay" });
  const [steps, setSteps] = useState<string[]>([]);
  const [status, setStatus] = useState("Checking local browser ↔ Rust host transport…");
  const [error, setError] = useState<string>();
  useEffect(() => {
    let client: EncryptedHubClient | undefined;
    let identity: DeviceIdentity | undefined;
    const signal = new AbortController();
    const add = (message: string) => setSteps((current) => [...current, message]);
    const fixture_url = new URLSearchParams(location.search).get("fixture") ?? "http://localhost:15579";
    const api = async <T,>(path: string, body?: unknown): Promise<T> => {
      const fixture = new URL(fixture_url);
      if (fixture.protocol !== "http:" || !["localhost", "127.0.0.1", "[::1]"].includes(fixture.hostname) || fixture.username || fixture.password || fixture.search || fixture.hash || fixture.pathname !== "/") throw new Error("This check requires an isolated loopback fixture.");
      const response = await fetch(`${fixture.origin}${path}`, { method: body === undefined ? "GET" : "POST", headers: body === undefined ? {} : { "Content-Type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body), signal: signal.signal });
      if (!response.ok) throw new Error(`Fixture returned ${response.status}`);
      return response.json();
    };
    void (async () => {
      const crypto = await loadHubCrypto(); identity = crypto.DeviceIdentity.generate();
      const config = await api<FixtureConfig>("/smoke/config");
      await api("/smoke/authorize", { device_public_key: identity.public_key() });
      add("Ephemeral test device authorized for synthetic fixture data");
      client = await EncryptedHubClient.connect(config.hub_url, { host_id: config.host_id, host_public_key: config.host_public_key }, identity, crypto, signal.signal);
      add("Encrypted Hub relay health authenticated");
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("Direct path was not established within 30 seconds")), 30_000);
        client!.setTransportListener((path) => {
          setTransport(path);
          if (path.kind === "direct") { clearTimeout(timer); resolve(); }
          else if (path.reason) { clearTimeout(timer); reject(new Error(path.reason)); }
        });
      });
      add("WebRTC direct health authenticated with fresh Noise IK and saved host pin");
      const large = await client.invoke<{ received_bytes: number }>("list_sessions", { large: "x".repeat(900_000) });
      if (large.received_bytes !== 900_000) throw new Error("Large direct request did not arrive intact");
      add("900,000-byte encrypted request arrived intact over direct data channels");
      await api("/smoke/stop-hub", {});
      const after = await client.invoke<{ received_bytes: number }>("list_sessions", { large: "x".repeat(70_000) });
      if (after.received_bytes !== 70_000) throw new Error("Direct request failed after Hub tunnels stopped");
      add("70,000-byte encrypted request succeeded after Hub tunnels stopped");
      setStatus("Passed: browser ↔ Rust host stays direct with the Hub relay offline");
    })().catch((failure) => { setError(String(failure)); setStatus("Local transport check failed"); });
    return () => { signal.abort(); client?.close(); identity?.free(); };
  }, []);
  return <main style={{ maxWidth: 880, margin: "70px auto", padding: 30 }}>
    <p style={{ color: "var(--text-muted)" }}>TOKN · LOCAL FIXTURE</p>
    <h1>Direct transport</h1>
    <h2 role="status">{status}</h2>
    <ol>{steps.map((step) => <li key={step} style={{ marginBlock: 16 }}>{step}</li>)}</ol>
    {error && <pre role="alert" style={{ whiteSpace: "pre-wrap" }}>{error}</pre>}
    <p>Real browser WebRTC · Rust host · WASM Noise encryption · no public STUN service</p>
    <div style={{ position: "fixed", bottom: 24, left: 24 }}><RemoteConnection name="Synthetic test machine" hub_url="Loopback Hub" state="connected" encrypted transport={transport}><button>Local test fixture</button></RemoteConnection></div>
  </main>;
}
createRoot(document.getElementById("root")!).render(<Smoke />);
