import { execFileSync } from "node:child_process";
import { readFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";

const viewer_root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const workspace_root = resolve(viewer_root, "../..");
const output_dir = join(viewer_root, "src/lib/hub-wasm");
const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--locked", "--no-deps", "--format-version", "1"], {
  cwd: workspace_root,
  encoding: "utf8",
})) as { target_directory: string };
const lock = readFileSync(join(workspace_root, "Cargo.lock"), "utf8");
const version = lock.match(/\[\[package\]\]\nname = "wasm-bindgen"\nversion = "([^"]+)"/)?.[1];
if (!version) throw new Error("Cargo.lock does not contain the pinned wasm-bindgen version.");

const bindgen_version = execFileSync("wasm-bindgen", ["--version"], { encoding: "utf8" }).trim();
if (bindgen_version !== `wasm-bindgen ${version}`) {
  throw new Error(`Install matching bindings: cargo install wasm-bindgen-cli --version ${version} --locked`);
}
const targets = execFileSync("rustup", ["target", "list", "--installed"], { encoding: "utf8" }).split(/\s+/);
if (!targets.includes("wasm32-unknown-unknown")) {
  throw new Error("Install the browser target: rustup target add wasm32-unknown-unknown");
}

execFileSync("cargo", ["build", "--locked", "--release", "--target", "wasm32-unknown-unknown", "-p", "tokn-hub-client-core"], {
  cwd: workspace_root,
  stdio: "inherit",
});
mkdirSync(output_dir, { recursive: true });
execFileSync("wasm-bindgen", [
  join(metadata.target_directory, "wasm32-unknown-unknown/release/tokn_hub_client_core.wasm"),
  "--target", "web", "--out-dir", output_dir, "--out-name", "tokn_hub_client_core",
], { cwd: workspace_root, stdio: "inherit" });
