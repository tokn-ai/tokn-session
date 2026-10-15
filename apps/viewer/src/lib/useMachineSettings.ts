import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { configureRelay, getMachineSettings, listenForRelayStatus } from "./tauri";
import { captureTransport, type CommandInvoker, type EventSubscriber } from "./transport";
import type { MachineSettingsSnapshot, RelaySettings, RelayStatus } from "./types";

const describe = (error: unknown) => error instanceof Error ? error.message : String(error);

/** All reads and writes stay bound to the machine that opened this page. */
export function useMachineSettings(local: boolean, transport_override?: { invoke: CommandInvoker; listen: EventSubscriber }, enabled = true) {
  const transport = useMemo(() => transport_override ?? captureTransport(), [transport_override]);
  const [settings, setSettings] = useState<MachineSettingsSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const alive = useRef(false);
  const lifetime = useRef(0);
  const revision = useRef(0);
  const latest_status = useRef<RelayStatus | null>(null);

  useEffect(() => {
    if (!enabled) { setSettings(null); setLoading(false); setError(null); return; }
    alive.current = true;
    lifetime.current += 1;
    let current = true;
    let stop: (() => void) | undefined;
    latest_status.current = null;
    setLoading(true);
    setError(null);
    void (async () => {
      try {
        const unlisten = await listenForRelayStatus((status) => {
          if (!current) return;
          revision.current += 1;
          latest_status.current = status;
          setSettings((previous) => previous && { ...previous, reader: { ...previous.reader, status } });
        }, transport.listen);
        if (!current) { unlisten(); return; }
        stop = unlisten;
      } catch { /* A snapshot and explicit Apply still work without status events. */ }
      if (!current) return;
      const before = revision.current;
      try {
        const snapshot = await getMachineSettings(transport.invoke);
        if (!current) return;
        if (snapshot.version !== 1) throw new Error("This host uses an unsupported settings format.");
        setSettings({ ...snapshot,
          reader: { ...snapshot.reader,
            editable: local && snapshot.reader.editable,
            status: revision.current !== before && latest_status.current ? latest_status.current : snapshot.reader.status },
          hosting: { ...snapshot.hosting, editable: local && snapshot.hosting.editable },
        });
      } catch (failure) {
        if (!current) return;
        const message = describe(failure);
        setError(/unknown.*command|unknown viewer api route|command.*not found/i.test(message)
          ? "This host does not expose machine settings. Update the host to view its configuration."
          : message);
      } finally { if (current) setLoading(false); }
    })();
    return () => { current = false; alive.current = false; lifetime.current += 1; stop?.(); };
  }, [attempt, enabled, local, transport]);

  const applyReader = useCallback(async (draft: RelaySettings) => {
    if (!local || !settings?.reader.editable) throw new Error("Reader settings are read-only for this machine.");
    const before = revision.current;
    const epoch = lifetime.current;
    const status = await configureRelay({ ...draft, endpoint: draft.endpoint.trim() }, transport.invoke);
    if (!alive.current || lifetime.current !== epoch) throw new Error("Machine settings changed while applying. Reload the settings to check the saved value.");
    if (revision.current === before) {
      revision.current += 1;
      latest_status.current = status;
      setSettings((previous) => previous && { ...previous, reader: { ...previous.reader, status } });
    }
  }, [local, settings?.reader.editable, transport]);

  const retry = useCallback(() => setAttempt((value) => value + 1), []);
  return { settings, loading, error, retry, applyReader };
}
