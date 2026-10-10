/** Rust owns protocol validation and crypto; this module only loads its WASM adapter. */
export interface DeviceIdentity {
  public_key(): string;
  export_secret(): string;
  free(): void;
}
export interface NoiseChannel {
  encrypt_json(message: string): Uint8Array;
  decrypt_json(record: Uint8Array): string;
  remote_public_key(): string;
  free(): void;
}
export interface CryptoApi {
  DeviceIdentity: {
    generate(): DeviceIdentity;
    from_secret(secret: string): DeviceIdentity;
  };
  ClientPairing: {
    start(host_id: string, identity: DeviceIdentity, code: string, now_seconds: number): {
      record(): Uint8Array;
      free(): void;
      confirm(record: Uint8Array): { record(): Uint8Array; finish(record: Uint8Array): string; free(): void };
    };
  };
  NoiseInitiator: {
    start(identity: DeviceIdentity, host_public_key: string): {
      record(): Uint8Array;
      free(): void;
      finish(record: Uint8Array): NoiseChannel;
    };
  };
}
let loaded: Promise<CryptoApi> | undefined;
export function loadHubCrypto(): Promise<CryptoApi> {
  loaded ??= import("./hub-wasm/tokn_hub_client_core").then(async (module) => {
    await module.default();
    return module as unknown as CryptoApi;
  }).catch((error: unknown) => {
    loaded = undefined;
    throw error;
  });
  return loaded;
}
