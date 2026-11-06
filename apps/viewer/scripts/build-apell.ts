import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFile, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "vite";
import react from "@vitejs/plugin-react";

const viewer = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repository = resolve(viewer, "../..");
const source = resolve(repository, "apps/apell");
const apell_source = process.env.APELL_SOURCE;
if (!apell_source) throw new Error("Set APELL_SOURCE to a checkout of Apell with the applet networking and secure-storage services.");
const apell = await realpath(apell_source);
const staging = await mkdtemp(join(tmpdir(), "tokn-session-apell-"));
const target = resolve(process.env.CARGO_TARGET_DIR ?? join(repository, "target"));
const artifact = join(repository, "artifacts/tokn-session.apel");
const check_only = process.argv.includes("--check");
try {
  const prior = process.env.CARGO_ENCODED_RUSTFLAGS?.split("\x1f") ?? (process.env.RUSTFLAGS ?? "").split(/\s+/).filter(Boolean);
  execFileSync("cargo", ["build", "--locked", "--release", "--target", "wasm32-unknown-unknown", "-p", "tokn-session-apell"], {
    cwd: repository, stdio: "inherit", env: { ...process.env,
      CARGO_ENCODED_RUSTFLAGS: [...prior, `--remap-path-prefix=${homedir()}=/build`, `--remap-path-prefix=${repository}=/tokn-session`, `--remap-path-prefix=${apell}=/apell`].join("\x1f") },
  });
  await build({
    configFile: false, root: source, base: "./", plugins: [react()],
    resolve: { alias: {
      "@apell/sdk": join(apell, "packages/applet-sdk/src/index.ts"),
      "react": join(viewer, "node_modules/react"),
      "react-dom": join(viewer, "node_modules/react-dom"),
    } },
    build: { outDir: staging, emptyOutDir: true, sourcemap: false },
  });
  await mkdir(join(staging, "lib"));
  const wasm = await readFile(join(target, "wasm32-unknown-unknown/release/tokn_session_apell.wasm"));
  if (wasm.byteLength > 16 * 1024 * 1024) throw new Error("The applet backend exceeds Apell's module limit.");
  const imports = WebAssembly.Module.imports(new WebAssembly.Module(wasm));
  if (imports.some((item) => item.module !== "apell" || !["send", "host_value"].includes(item.name))) {
    throw new Error("The applet backend imports a browser or native runtime.");
  }
  await writeFile(join(staging, "lib/main.wasm"), wasm);
  await copyFile(join(source, "manifest.json"), join(staging, "manifest.json"));
  const manifest = JSON.parse(await readFile(join(staging, "manifest.json"), "utf8")) as { applet_id: string; lib: string; permissions: string[] };
  if (manifest.applet_id !== "tokn-session" || manifest.lib !== "lib/main.wasm"
    || ["http", "config", "websocket", "secrets", "crypto", "runtime"].some((permission) => !manifest.permissions.includes(permission))) {
    throw new Error("The applet manifest does not declare its required backend services.");
  }
  if (check_only) {
    process.stdout.write(`Checked shared applet entry, manifest and portable backend (${wasm.byteLength} bytes).\n`);
  } else {
    await mkdir(dirname(artifact), { recursive: true });
    execFileSync("cargo", ["run", "--locked", "--manifest-path", join(apell, "Cargo.toml"), "-p", "apell-cli", "--", "pack", staging, "--output", artifact], { stdio: "inherit" });
    const digest = createHash("sha256").update(await readFile(artifact)).digest("hex");
    await writeFile(artifact.replace(/\.apel$/, ".sha256"), `${digest}  tokn-session.apel\n`);
    process.stdout.write(`Built Tokn Session applet (${wasm.byteLength} backend bytes). SHA-256 ${digest}\n`);
  }
} finally {
  await rm(staging, { recursive: true, force: true });
}
