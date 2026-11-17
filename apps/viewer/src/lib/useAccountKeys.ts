import { useEffect, useRef, useState } from "react";
import type { HubClient } from "./hub";
import { loadAccountKeys, type AccountKey } from "./hubAccount";

interface KeyList {
  client: HubClient;
  keys?: AccountKey[];
  error?: string;
}

/** Only key metadata is cached, keyed by the current authenticated client.
 * One-time secrets and dialog drafts belong to the mounted profile dialog. */
export function useAccountKeys(client: HubClient | undefined, visible: boolean, on_failure: (error: unknown, report: (message: string) => void) => void) {
  const [snapshot, setSnapshot] = useState<KeyList>();
  const [attempt, setAttempt] = useState(0);
  const read_generation = useRef(0);
  const current = snapshot?.client === client ? snapshot : undefined;

  useEffect(() => {
    setSnapshot((previous) => previous?.client === client ? previous : undefined);
    if (!visible || !client) return;
    const generation = ++read_generation.current;
    const live = () => generation === read_generation.current && !client.signal.aborted;
    void loadAccountKeys(client).then((keys) => {
      if (live()) setSnapshot({ client, keys });
    }).catch((error: unknown) => {
      if (live()) on_failure(error, (message) => setSnapshot((previous) => ({
        client, keys: previous?.client === client ? previous.keys : undefined, error: message,
      })));
    });
    return () => { read_generation.current++; };
  }, [client, visible, attempt, on_failure]);

  function refresh() {
    read_generation.current++;
    setAttempt((value) => value + 1);
  }

  function updateKeys(update: (keys?: AccountKey[]) => AccountKey[] | undefined) {
    if (!client || client.signal.aborted) return;
    setSnapshot((previous) => ({ client, keys: update(previous?.client === client ? previous.keys : undefined) }));
    refresh();
  }

  return {
    keys: current?.keys, error: current?.error, refresh,
    created: (key: AccountKey) => updateKeys((previous) => [key, ...(previous ?? []).filter((entry) => entry.key_id !== key.key_id)]),
    revoked: (key_id: string) => updateKeys((previous) => previous?.filter((entry) => entry.key_id !== key_id)),
  };
}

export type AccountKeyList = ReturnType<typeof useAccountKeys>;
