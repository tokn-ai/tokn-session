export type { HubNamespace } from "./types";

export interface HubStatus {
  configured: boolean;
  authenticated: boolean;
}

export interface HubHost {
  host_id: string;
  name: string;
  online: boolean;
  access: "view" | "control";
  secure_only?: boolean;
  machine_address?: string;
}

export interface HubEnrollment {
  pairing_code: string;
  host_id: string;
  name: string;
  access: "view" | "control";
  expires_in: number;
}

interface Ceremony<T> {
  ceremony_id: string;
  options: { publicKey: T };
}

// These nested field names are the WebAuthn standard wire format, shared with
// the authenticator and webauthn-rs. Hub's own envelopes use snake_case.
type CredentialDescriptor = Omit<PublicKeyCredentialDescriptor, "id"> & { id: string };
type CreationOptions = Omit<PublicKeyCredentialCreationOptions, "challenge" | "user" | "excludeCredentials"> & {
  challenge: string;
  user: Omit<PublicKeyCredentialUserEntity, "id"> & { id: string };
  excludeCredentials?: CredentialDescriptor[];
};
type RequestOptions = Omit<PublicKeyCredentialRequestOptions, "challenge" | "allowCredentials"> & {
  challenge: string;
  allowCredentials?: CredentialDescriptor[];
};

export class HubError extends Error {
  constructor(message: string, readonly status: number) { super(message); }
}

export async function detectHub(signal: AbortSignal): Promise<HubStatus | null> {
  const response = await fetch(`${window.location.origin}/hub/v1/auth/status`, {
    signal, credentials: "omit", redirect: "error", headers: { Accept: "application/json" },
  });
  if (response.status === 404) return null;
  // Vite and direct viewer servers may return the SPA for unknown paths.
  if (response.ok && response.headers.get("content-type")?.includes("text/html")) return null;
  if (!response.ok) throw new Error(`Unable to reach Hub (${response.status}).`);
  const status: HubStatus = await response.json();
  if (typeof status.configured !== "boolean" || typeof status.authenticated !== "boolean") {
    throw new Error("The server returned an invalid Hub status.");
  }
  return status;
}

export function decodeBase64Url(value: string): ArrayBuffer {
  const padded = value.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(value.length / 4) * 4, "=");
  return Uint8Array.from(atob(padded), (character) => character.charCodeAt(0)).buffer;
}

export function encodeBase64Url(value: ArrayBuffer): string {
  let binary = "";
  for (const byte of new Uint8Array(value)) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function creationOptions(options: CreationOptions): PublicKeyCredentialCreationOptions {
  return {
    ...options,
    challenge: decodeBase64Url(options.challenge),
    user: { ...options.user, id: decodeBase64Url(options.user.id) },
    excludeCredentials: options.excludeCredentials?.map((credential) => ({ ...credential, id: decodeBase64Url(credential.id) })),
  };
}

export function requestOptions(options: RequestOptions): PublicKeyCredentialRequestOptions {
  return {
    ...options,
    challenge: decodeBase64Url(options.challenge),
    allowCredentials: options.allowCredentials?.map((credential) => ({ ...credential, id: decodeBase64Url(credential.id) })),
  };
}

export function credentialJson(credential: PublicKeyCredential): Record<string, unknown> {
  const response = credential.response;
  const common = { clientDataJSON: encodeBase64Url(response.clientDataJSON) };
  return {
    id: credential.id,
    rawId: encodeBase64Url(credential.rawId),
    type: credential.type,
    authenticatorAttachment: credential.authenticatorAttachment,
    extensions: credential.getClientExtensionResults(),
    response: "attestationObject" in response ? {
      ...common,
      attestationObject: encodeBase64Url((response as AuthenticatorAttestationResponse).attestationObject),
      transports: (response as AuthenticatorAttestationResponse).getTransports?.() ?? [],
    } : {
      ...common,
      authenticatorData: encodeBase64Url((response as AuthenticatorAssertionResponse).authenticatorData),
      signature: encodeBase64Url((response as AuthenticatorAssertionResponse).signature),
      userHandle: (response as AuthenticatorAssertionResponse).userHandle === null
        ? null : encodeBase64Url((response as AuthenticatorAssertionResponse).userHandle!),
    },
  };
}

export class HubClient {
  private lifetime = new AbortController();
  constructor(private token = "", readonly endpoint = window.location.origin) {}
  get signal(): AbortSignal { return this.lifetime.signal; }
  get access_token(): string { return this.token; }

  async request<T>(path: string, method = "GET", body?: unknown): Promise<T> {
    if (this.signal.aborted) throw new Error("Hub disconnected");
    const controller = new AbortController();
    const abort = () => controller.abort();
    this.signal.addEventListener("abort", abort, { once: true });
    const timeout = setTimeout(abort, 30_000);
    try {
      const response = await fetch(`${this.endpoint}/hub/v1/${path}`, {
        method,
        headers: {
          Accept: "application/json",
          ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
          ...(body === undefined ? {} : { "Content-Type": "application/json" }),
        },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
        credentials: "omit",
        redirect: "error",
      });
      if (!response.ok) {
        const error = await response.json().catch(() => ({})) as { error?: string };
        throw new HubError(error.error ?? `Hub returned ${response.status}`, response.status);
      }
      return response.status === 204 ? undefined as T : await response.json() as T;
    } finally {
      clearTimeout(timeout);
      this.signal.removeEventListener("abort", abort);
    }
  }

  async authenticate(register: boolean, bootstrap_token?: string): Promise<HubClient> {
    if (!window.PublicKeyCredential || !navigator.credentials) {
      throw new Error("Passkeys require a supported browser and HTTPS (or localhost).");
    }
    const flow = register ? "register" : "login";
    const start = await this.request<Ceremony<CreationOptions | RequestOptions>>(`auth/${flow}/start`, "POST", { bootstrap_token });
    const credential = register
      ? await navigator.credentials.create({ publicKey: creationOptions(start.options.publicKey as CreationOptions), signal: this.signal })
      : await navigator.credentials.get({ publicKey: requestOptions(start.options.publicKey as RequestOptions), signal: this.signal });
    if (!credential) throw new Error("No passkey was selected. Try again.");
    const session = await this.request<{ access_token: string }>(`auth/${flow}/finish`, "POST", {
      ceremony_id: start.ceremony_id,
      credential: credentialJson(credential as PublicKeyCredential),
    });
    return new HubClient(session.access_token, this.endpoint);
  }

  close() {
    this.lifetime.abort();
    this.token = "";
  }
}

export function hubErrorMessage(error: unknown): string {
  if (error instanceof DOMException && error.name === "NotAllowedError") {
    return "The passkey request was cancelled or timed out. Try again when you are ready.";
  }
  return error instanceof Error ? error.message : String(error);
}
