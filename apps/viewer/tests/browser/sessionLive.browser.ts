import { expect, type Page } from "@playwright/test";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { SessionControlStatus, SessionInputStatus, SessionModelStatus } from "../../src/lib/types";
import { serveFixture, test as base } from "./sessionFixture";

const title = "Review parser edge cases";
const firstPrompt = "Which parser inputs do the tests already cover?";
const latestPrompt = "Run the parser tests with those additional cases.";
const liveText = "The whitespace and empty-input cases pass. I’m checking that quoted arguments still preserve their spacing.";

const test = base.extend<{ live_url: string }>({
  live_url: async ({ fixture_home }, use) => {
    const project = join(fixture_home, ".qoder", "projects", "-workspace-parser");
    await mkdir(project, { recursive: true });
    const sessionId = "parser-review";
    const transcript = [
      firstPrompt,
      [
        "Four parser cases already have coverage.",
        "- **Option values** — a flag can take its value from the following argument.",
        "- **Quoted arguments** — spaces inside quotes stay in one token.",
        "- **Positional arguments** — values keep their original order.",
        "- **End of options** — arguments after `--` are treated as positional.",
      ].join("\n\n"),
      "Where is the whitespace case handled?",
      "The lexer trims whitespace before splitting arguments. Quoted whitespace passes through a separate branch.",
      "What should the regression tests assert?",
      "An empty string and an input containing only spaces should both produce zero tokens. A quoted space should produce one.",
      latestPrompt,
    ].map((text, index) => {
      const role = index % 2 === 0 ? "user" : "assistant";
      return { type: role, uuid: `message-${index}`, parentUuid: index ? `message-${index - 1}` : null,
        sessionId, cwd: "/workspace/parser", timestamp: new Date(Date.UTC(2026, 0, 12, 10, index)).toISOString(),
        message: role === "user" ? { role, content: text }
          : { role, id: `reply-${index}`, stop_reason: "end_turn", content: [{ type: "text", text }] } };
    });
    await writeFile(join(project, `${sessionId}.jsonl`), [
      transcript[0], { type: "custom-title", sessionId, customTitle: title }, ...transcript.slice(1),
    ].map(record => JSON.stringify(record)).join("\n") + "\n");
    await serveFixture(fixture_home, use);
  },
});

/** The actual Qoder history comes from the local API. Only the active runtime is simulated. */
async function openBeforeStreaming(page: Page, url: string) {
  let start!: () => void;
  const ready = new Promise<void>(resolve => { start = resolve; });
  await page.route("**/api/v1/get_session_control", async route => {
    await ready;
    const status: SessionControlStatus = {
      session_key: route.request().postDataJSON().request.session_key, provider: "qoder", transport: "rpc",
      can_select: false, can_cancel: true, available: true, is_running: true, message: "",
      runtime_id: "browser-runtime", instance_id: "browser-instance",
      live_output: { prompt_id: "test-parser", truncated: false, parts: [
        { kind: "reasoning", text: "Start with the focused lexer tests." },
        { kind: "tool", text: "cargo test -p parser", status: "running" },
        { kind: "text", text: liveText },
      ] },
    };
    await route.fulfill({ json: status });
  });
  const input: SessionInputStatus = { available: true, max_length: 8000, message: "", delivery_modes: ["queue"] };
  await page.route("**/api/v1/get_session_input_status", route => route.fulfill({ json: input }));
  const model: SessionModelStatus = {
    state: { model: null, source: "unknown", revision: "active" }, can_select: false, can_list: false,
    reason: null, runtime_generation: "browser-instance", updates_pi_default: false,
  };
  await page.route("**/api/v1/get_session_model_status", route => route.fulfill({ json: model }));
  await page.goto(url, { waitUntil: "domcontentloaded" });
  await page.getByRole("main", { name: "Session workspace" }).getByRole("button", { name: new RegExp(title) }).click();
  const history = page.getByRole("region", { name: "Conversation history" });
  await expect(history.getByText(latestPrompt, { exact: true })).toBeVisible();
  await expect(history.getByRole("region", { name: "Live response" })).toHaveCount(0);
  return { start, history };
}

test("follows live output in the same column as saved turns", async ({ page, live_url }, info) => {
  const { start, history } = await openBeforeStreaming(page, live_url);
  start();
  const live = history.getByRole("region", { name: "Live response" });
  await expect(live.getByText(liveText)).toBeInViewport();
  await expect(live.getByRole("status")).toBeInViewport();
  await expect(page.getByRole("button", { name: "Stop", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Jump to latest" })).toHaveCount(0);
  expect(await history.evaluate(node => node.scrollHeight - node.clientHeight - node.scrollTop)).toBeLessThanOrEqual(2);
  const prompt = history.getByText(latestPrompt, { exact: true });
  const promptBox = await prompt.boundingBox();
  const liveBox = await live.boundingBox();
  expect(liveBox!.y).toBeGreaterThanOrEqual(promptBox!.y + promptBox!.height);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(page.viewportSize()!.width);
  await page.mouse.move(0, 0);
  await page.screenshot({ path: info.outputPath(`${info.project.name}-live-response.png`), animations: "disabled" });
});

test("holds the reading line when live output starts and resumes on Jump to latest", async ({ page, live_url }) => {
  const { start, history } = await openBeforeStreaming(page, live_url);
  await history.getByRole("button", { name: "Load earlier turns" }).click();
  await expect(history.getByText(firstPrompt, { exact: true })).toBeAttached();
  await page.getByRole("button", { name: "Jump to prompt" }).click();
  await page.getByRole("region", { name: "Prompts in this session" }).getByRole("button").filter({ hasText: firstPrompt }).click();
  const first = history.locator("[data-reading-prompt]").first();
  const top = await first.evaluate(node => node.getBoundingClientRect().top);
  start();
  const live = history.getByRole("region", { name: "Live response" });
  await expect(live.getByText(liveText)).toBeAttached();
  await expect(live.getByRole("status")).not.toBeInViewport();
  expect(Math.abs((await first.evaluate(node => node.getBoundingClientRect().top)) - top)).toBeLessThanOrEqual(2);
  await page.getByRole("button", { name: /Jump to latest/ }).click();
  await expect(live.getByRole("status")).toBeInViewport();
  await expect(history).toBeFocused();
});
