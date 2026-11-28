import { cp } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export function installDemoRollout(home: string) {
  const fixture = fileURLToPath(new URL("../fixtures/session-reader/", import.meta.url));
  return cp(fixture, join(home, ".codex"), { recursive: true });
}

/** Keep provider discovery, machine settings and temporary files inside the demo. */
export function isolatedViewerEnvironment(home: string): NodeJS.ProcessEnv {
  return {
    HOME: home, USERPROFILE: home, APPDATA: home, LOCALAPPDATA: home,
    TMPDIR: home, TEMP: home, TMP: home, SystemRoot: process.env.SystemRoot,
    // The demo is a saved rollout. Do not discover the host's bundled Codex.
    TOKN_SESSION_CODEX_EXECUTABLE: join(home, "unavailable", "codex"),
  };
}
