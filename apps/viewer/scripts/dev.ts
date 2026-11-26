import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { createServer, type ViteDevServer } from "vite";
import { installDemoRollout, isolatedViewerEnvironment } from "./session-demo";

const root = fileURLToPath(new URL("../", import.meta.url));
let child: ChildProcess | undefined;
let vite: ViteDevServer | undefined;
let stopping = false;
let demoHome: string | undefined;
let apiEnded: Promise<void> | undefined;

async function stop() {
  if (stopping) return;
  stopping = true;
  child?.kill("SIGINT");
  const timer = setTimeout(() => child?.kill("SIGKILL"), 5000);
  timer.unref();
  await vite?.close();
}
process.on("SIGINT", () => { void stop(); });
process.on("SIGTERM", () => { void stop(); });

function buildApi(): Promise<string> {
  return new Promise((resolve, reject) => {
    const build = spawn("cargo", ["build", "-p", "tokn-viewer-api", "--message-format=json"], {
      cwd: root,
      stdio: ["ignore", "pipe", "inherit"],
    });
    child = build;
    let executable: string | undefined;
    createInterface({ input: build.stdout }).on("line", (line) => {
      try {
        const message = JSON.parse(line);
        if (message.reason === "compiler-artifact" && message.target.name === "tokn-viewer-api") {
          executable = message.executable ?? executable;
        }
        if (message.reason === "compiler-message" && message.message.rendered) {
          process.stderr.write(message.message.rendered);
        }
      } catch { process.stderr.write(`${line}\n`); }
    });
    build.once("error", reject);
    build.once("close", (code) => {
      child = undefined;
      if (code === 0 && executable) resolve(executable);
      else reject(new Error("Could not build viewer-api"));
    });
  });
}

async function main() {
  const executable = await buildApi();
  if (stopping) return;
  const token = randomBytes(32).toString("hex");
  const args = ["--api-only", "--bind", "127.0.0.1:0"];
  if (process.argv.includes("--demo")) {
    demoHome = await mkdtemp(join(tmpdir(), "tokn-reader-demo-"));
    await installDemoRollout(demoHome);
    args.push("--local", "--native", "--index-path", join(demoHome, "index.sqlite"));
  }
  if (stopping) return;
  const api = spawn(executable, args, {
    cwd: demoHome ?? root,
    env: { ...(demoHome ? isolatedViewerEnvironment(demoHome) : process.env), TOKN_VIEWER_TOKEN: token },
    stdio: ["ignore", "inherit", "pipe"],
  });
  child = api;
  apiEnded = new Promise<void>((resolve) => {
    api.once("close", (code) => {
      child = undefined;
      if (!stopping) {
        process.exitCode = code || 1;
        console.error("Viewer API stopped; closing Vite.");
        void stop();
      }
      resolve();
    });
  });
  const endpoint = await new Promise<string>((resolve, reject) => {
    api.once("error", reject);
    api.once("close", () => reject(new Error("Viewer API exited before becoming ready")));
    createInterface({ input: api.stderr }).on("line", (line) => {
      console.error(line);
      const match = /^Viewer listening on (http:\/\/127\.0\.0\.1:\d+)$/.exec(line);
      if (match) resolve(match[1]);
    });
  });
  if (stopping) return;
  vite = await createServer({
    root,
    server: {
      host: "127.0.0.1",
      port: Number(process.env.TOKN_VIEWER_DEV_PORT ?? (demoHome ? 1438 : 1437)),
      strictPort: !demoHome || !!process.env.TOKN_VIEWER_DEV_PORT,
      proxy: { "/api": { target: endpoint } },
    },
  });
  if (stopping) { await vite.close(); return; }
  await vite.listen();
  const url = vite.resolvedUrls?.local[0];
  if (!url) throw new Error("Vite did not provide a local address");
  console.log(`\nOpen the viewer: ${url}#token=${encodeURIComponent(token)}\n`);
  await apiEnded;
}

try { await main(); }
catch (error) {
  if (!stopping) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
} finally {
  await stop();
  await apiEnded;
  if (demoHome) await rm(demoHome, { recursive: true, force: true });
}
