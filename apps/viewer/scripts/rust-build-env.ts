import { homedir } from "node:os";

/** Keep local compiler paths out of distributed WASM and preserve caller flags. */
export function rustBuildEnvironment(repository: string, additional_sources: [string, string][] = []): NodeJS.ProcessEnv {
  const existing = process.env.CARGO_ENCODED_RUSTFLAGS?.split("\x1f")
    ?? (process.env.RUSTFLAGS ?? "").split(/\s+/).filter(Boolean);
  const sources: [string, string][] = [[homedir(), "/build"], [repository, "/tokn-session"], ...additional_sources];
  return {
    ...process.env,
    CARGO_ENCODED_RUSTFLAGS: [
      ...existing,
      ...sources.map(([source, destination]) => `--remap-path-prefix=${source}=${destination}`),
    ].join("\x1f"),
  };
}
