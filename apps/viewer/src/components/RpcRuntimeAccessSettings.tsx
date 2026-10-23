import { useEffect, useId, useRef, useState } from "react";
import { getRpcRuntimeAccess, setRpcRuntimeAccess } from "../lib/tauri";
import type { RpcRuntimeAccess } from "../lib/types";

export function RpcRuntimeAccessSettings({ active }: { active: boolean }) {
  const id = useId();
  const [access, setAccess] = useState<RpcRuntimeAccess | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const generation = useRef(0);
  const alive = useRef(true);
  const pending = useRef(false);
  useEffect(() => { alive.current = true; return () => { alive.current = false; generation.current += 1; }; }, []);
  useEffect(() => {
    if (!active || pending.current) return;
    let current = true;
    const read = ++generation.current;
    void getRpcRuntimeAccess().then((value) => { if (current && generation.current === read) { setAccess(value); setError(null); } }).catch((failure) => {
      if (current && generation.current === read) setError(failure instanceof Error ? failure.message : String(failure));
    });
    return () => { current = false; };
  }, [active]);
  async function change(enabled: boolean) {
    if (pending.current) return;
    pending.current = true;
    generation.current += 1;
    setBusy(true); setError(null);
    try { const value = await setRpcRuntimeAccess({ enabled }); if (alive.current) setAccess(value); }
    catch (failure) { if (alive.current) setError(failure instanceof Error ? failure.message : String(failure)); }
    finally { pending.current = false; if (alive.current) setBusy(false); }
  }
  return <div className="settings-page__card"><div className="settings-page__row">
    <div><label htmlFor={id}>Remote runtime management</label><p id={`${id}-description`}>Allow paired clients with agent control to configure and launch runtimes on this computer.</p></div>
    <input id={id} type="checkbox" checked={access?.enabled ?? false} disabled={busy || !access?.available} onChange={(event) => { void change(event.currentTarget.checked); }} aria-describedby={`${id}-description`} />
  </div>{access?.reason && <p className="settings-page__hint">{access.reason}</p>}{error && <p className="settings-page__error" role="alert">{error}</p>}</div>;
}
