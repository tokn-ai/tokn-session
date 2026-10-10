import { useState } from "react";
import { browserPasskeys } from "../lib/hubEncryptedClient";
import { decodeBase64Url, hubErrorMessage } from "../lib/hub";

export interface NativePasskeyRequest {
  operation: "register" | "login";
  options: { publicKey: Parameters<typeof browserPasskeys.create>[0] };
  callback_url: string;
}
export function parseNativePasskeyRequest(fragment: string): NativePasskeyRequest {
  if (fragment.length > 128 * 1024) throw new Error("Passkey request exceeds its size limit.");
  const encoded = new URLSearchParams(fragment.replace(/^#/, "")).get("request");
  if (!encoded) throw new Error("Open the passkey link from the Tokn app.");
  const request = JSON.parse(new TextDecoder().decode(decodeBase64Url(encoded))) as NativePasskeyRequest;
  if (!request || !["register", "login"].includes(request.operation) || !request.options?.publicKey) throw new Error("Invalid app passkey request.");
  const callback = new URL(request.callback_url);
  if (callback.protocol !== "http:" || callback.hostname !== "127.0.0.1" || !callback.port
    || callback.username || callback.password || callback.search || callback.hash
    || !/^\/passkey\/[A-Za-z0-9_-]{43}$/.test(callback.pathname)) throw new Error("Invalid app passkey return address.");
  return request;
}
let captured: { request?: NativePasskeyRequest; error?: string } | undefined;
function captureRequest() {
  if (captured) return captured;
  try { captured = { request: parseNativePasskeyRequest(window.location.hash) }; }
  catch (error) { captured = { error: hubErrorMessage(error) }; }
  window.history.replaceState(window.history.state, "", `${window.location.pathname}${window.location.search}`);
  return captured;
}
function returnToApp(callback_url: string, field: "credential" | "error", value: string) {
  const form = document.createElement("form");
  form.method = "POST"; form.action = callback_url;
  const input = document.createElement("input");
  input.type = "hidden"; input.name = field; input.value = value;
  form.append(input); document.body.append(form); form.submit();
}

export function NativePasskey() {
  const [{ request, error: initial_error }] = useState(captureRequest);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(initial_error);
  async function authenticate() {
    if (!request) return;
    setBusy(true); setError(undefined);
    const controller = new AbortController();
    try {
      const credential = request.operation === "register"
        ? await browserPasskeys.create(request.options.publicKey, controller.signal)
        : await browserPasskeys.get(request.options.publicKey, controller.signal);
      returnToApp(request.callback_url, "credential", JSON.stringify(credential));
    } catch (failure) { setError(hubErrorMessage(failure)); setBusy(false); }
  }
  return <main className="machine-connect"><section className="native-passkey">
    <p className="hub-eyebrow">Tokn app</p><h1>{request?.operation === "register" ? "Add a machine passkey" : "Sign in to your machine"}</h1>
    <p>Continue the passkey request started by your Tokn app. The result returns to that app on this device.</p>
    {error && <p role="alert">{error}</p>}
    {request && <><button disabled={busy} onClick={() => { void authenticate(); }}>{busy ? "Waiting for passkey…" : request.operation === "register" ? "Create passkey for app" : "Sign in for app"}</button>
      <button disabled={busy} onClick={() => returnToApp(request.callback_url, "error", "The app passkey request was cancelled.")}>Cancel</button></>}
  </section></main>;
}
