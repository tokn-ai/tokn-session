import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { createApellHost, type AppletBridge } from "./lib/apellHost";
import { installViewerHost } from "./lib/platform";
import { installViewportSizing } from "./lib/viewport";

/** Apell packages this entry together with the same viewer used by desktop/web. */
export function mountApellViewer(bridge: AppletBridge, element: HTMLElement): () => void {
  const host = createApellHost(bridge);
  const restore = installViewerHost(host);
  const stop_viewport = installViewportSizing();
  const root = createRoot(element);
  root.render(<StrictMode><App /></StrictMode>);
  return () => { root.unmount(); stop_viewport(); host.dispose?.(); restore(); };
}
