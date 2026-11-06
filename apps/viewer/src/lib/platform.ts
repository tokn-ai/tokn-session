/** Host services are independent of viewport size and the selected machine. */
export type HostUnlisten = () => void;
export interface ViewerHost {
  kind: "desktop" | "apell" | "browser";
  capabilities: {
    local_machine: boolean;
    device_preferences: boolean;
    native_hub: boolean;
    passkeys: boolean;
    native_translation: boolean;
    runtime_management: boolean;
    hosting: boolean;
  };
  invoke<T>(command: string, payload?: Record<string, unknown>): Promise<T>;
  listen<T>(name: string, handler: (event: { payload: T }) => void): Promise<HostUnlisten>;
  dispose?(): void;
}

let installed: ViewerHost | undefined;

const desktop: ViewerHost = {
  kind: "desktop",
  capabilities: { local_machine: true, device_preferences: true, native_hub: true,
    passkeys: true, native_translation: true, runtime_management: true, hosting: true },
  invoke: async (command, payload) => {
    const { invoke } = await import("@tauri-apps/api/core");
    return payload === undefined ? invoke(command) : invoke(command, payload);
  },
  listen: async (name, handler) => (await import("@tauri-apps/api/event")).listen(name, handler),
};
const browser: ViewerHost = {
  kind: "browser",
  capabilities: { local_machine: false, device_preferences: false, native_hub: false,
    passkeys: true, native_translation: false, runtime_management: false, hosting: false },
  invoke: async () => { throw new Error("This host service is unavailable in the browser."); },
  listen: async () => { throw new Error("This host service is unavailable in the browser."); },
};

/** Configure before rendering. Tests can restore a prior host without global flags. */
export function installViewerHost(host: ViewerHost): HostUnlisten {
  const previous = installed;
  installed = host;
  return () => { if (installed === host) installed = previous; };
}

export function viewerHost(): ViewerHost {
  return installed ?? ("__TAURI_INTERNALS__" in window ? desktop : browser);
}
export const viewerCapabilities = () => viewerHost().capabilities;
export const isDesktopHost = () => viewerHost().kind === "desktop";
export const invokeHost: ViewerHost["invoke"] = (command, payload) => viewerHost().invoke(command, payload);
export const listenHost: ViewerHost["listen"] = (name, handler) => viewerHost().listen(name, handler);
