import { useEffect, type ReactNode } from "react";
import { initializeNativeDevicePreferences, useNativeDevicePreferencesStatus } from "../lib/devicePreferences";
import { isDesktop } from "../lib/transport";
import { NativePreferenceSaveNotice } from "./NativePreferenceSaveNotice";

/** Native configuration must be hydrated before opening sessions or local hosting. */
export function NativeDevicePreferencesProvider({ children }: { children: ReactNode }) {
  const native = isDesktop();
  const status = useNativeDevicePreferencesStatus();
  useEffect(() => {
    if (native) void initializeNativeDevicePreferences().catch(() => { /* The startup state exposes errors and Retry. */ });
  }, [native]);
  if (!native) return children;
  if (!status.ready) return <main className="hub-home hub-access"><div className="hub-content">
    <header className="hub-header"><div><p className="hub-eyebrow">Tokn</p><h1>Device preferences</h1></div></header>
    {status.error ? <>
      <p role="alert">Could not load this device’s preferences: {status.error}</p>
      <p className="hub-muted">Your saved files are preserved. Fix the file or storage access, then retry.</p>
      <button onClick={() => void initializeNativeDevicePreferences().catch(() => {})}>Retry</button>
    </> : <p role="status">Loading this device’s preferences…</p>}
  </div></main>;
  return <><NativePreferenceSaveNotice placement="workspace" />{children}</>;
}
