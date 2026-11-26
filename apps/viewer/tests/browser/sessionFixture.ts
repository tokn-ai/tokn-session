import { test as base } from "@playwright/test";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { appendFile, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../../", import.meta.url));
export const title = "Review parser behavior";
export const promptEnd = "Preserve the order of positional arguments.";
export const replyEnd = "All parser checks passed.";

async function writeSession(home: string) {
  const id = randomUUID();
  const now = new Date();
  const day = now.toISOString().slice(0, 10);
  const codex = join(home, ".codex");
  const history = join(codex, "sessions", ...day.split("-"));
  await mkdir(history, { recursive: true });
  const file = join(history, `rollout-${day}T00-00-00-${id}.jsonl`);
  const records: string[] = [];
  let sequence = 0;
  function record(type: string, payload: Record<string, unknown>) {
    const timestamp = new Date(now.getTime() + sequence++).toISOString();
    records.push(JSON.stringify({ timestamp, type, payload }));
  }
  async function appendTurn(prompt: string, reply: string) {
    const turn_id = randomUUID();
    record("event_msg", { type: "task_started", turn_id });
    record("event_msg", { type: "user_message", message: prompt });
    record("response_item", {
      type: "message", role: "assistant", phase: "final_answer",
      content: [{ type: "output_text", text: reply }],
    });
    record("event_msg", { type: "task_complete", turn_id });
    await appendFile(file, records.join("\n") + "\n");
    records.length = 0;
  }
  record("session_meta", { id, cwd: home, timestamp: now.toISOString() });
  await appendTurn("Review empty inputs. ".repeat(1000) + promptEnd, "The parser accepts empty input. ".repeat(600) + replyEnd);
  await writeFile(join(codex, "session_index.jsonl"),
    JSON.stringify({ id, thread_name: title, updated_at: now.toISOString() }) + "\n");
  return appendTurn;
}

interface ViewerFixture {
  url: string;
  appendTurn: (prompt: string, reply: string) => Promise<void>;
}

export const test = base.extend<{ viewer: ViewerFixture }>({
  viewer: async ({}, use) => {
    const home = await mkdtemp(join(tmpdir(), "session-reader-"));
    try {
      const appendTurn = await writeSession(home);
      const token = randomUUID();
      const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
      const executable = process.platform === "win32" ? "tokn-viewer-api.exe" : "tokn-viewer-api";
      const server = spawn(join(target, "debug", executable), [
        "--local", "--bind", "127.0.0.1:0", "--web-root", join(root, "apps/viewer/dist"),
        "--index-path", join(home, "index.sqlite"),
      ], {
        cwd: home,
        env: { HOME: home, USERPROFILE: home, APPDATA: home, LOCALAPPDATA: home, TMPDIR: home,
          SystemRoot: process.env.SystemRoot, TOKN_VIEWER_TOKEN: token },
        stdio: ["ignore", "ignore", "pipe"],
      });
      const closed = new Promise<void>(done => server.once("close", () => done()));
      try {
        const endpoint = await new Promise<string>((ready, reject) => {
          server.once("error", reject);
          server.once("exit", code => reject(new Error(`Viewer API exited during startup (${code})`)));
          createInterface({ input: server.stderr }).on("line", line => {
            if (line.startsWith("Viewer listening on ")) ready(line.slice("Viewer listening on ".length));
            else process.stderr.write(line + "\n");
          });
        });
        await use({ url: `${endpoint}/#token=${token}`, appendTurn });
      } finally {
        server.kill();
        await closed;
      }
    } finally {
      await rm(home, { recursive: true, force: true });
    }
  },
});
