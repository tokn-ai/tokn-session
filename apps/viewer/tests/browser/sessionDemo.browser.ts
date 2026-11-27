import { expect, type Page, type TestInfo } from "@playwright/test";
import { test } from "./sessionFixture";

const firstPrompt = "Reproduce the whitespace-only input failure.";
const middlePrompt = "Fix the empty-input check";
const lastPrompt = "Run the full parser suite";

async function openDemo(page: Page, url: string) {
  await page.goto(url, { waitUntil: "domcontentloaded" });
  await page.getByRole("main", { name: "Session workspace" }).getByRole("button", { name: /Fix whitespace tokenization/ }).click();
  await expect(page.getByText("All 18 tests pass.", { exact: true })).toBeVisible();
}

async function capture(page: Page, info: TestInfo, name: string) {
  await page.mouse.move(0, 0);
  await page.screenshot({ path: info.outputPath(`${info.project.name}-${name}.png`), animations: "disabled" });
}

test("searches the recorded prompts and steps between turns", async ({ page, demo_url }, info) => {
  await openDemo(page, demo_url);
  const index = page.getByRole("button", { name: "Jump to prompt" });
  await index.click();
  const prompts = page.getByRole("region", { name: "Prompts in this session" });
  await expect(prompts.getByRole("button").filter({ hasText: lastPrompt })).toBeVisible();
  await capture(page, info, "prompt-index");

  const search = prompts.getByRole("searchbox");
  await search.fill("empty-input check");
  await expect(prompts.getByRole("button").filter({ hasText: middlePrompt })).toBeVisible();
  await search.press("Enter");
  const rows = page.locator("[data-reading-prompt]");
  await expect(rows.filter({ hasText: middlePrompt })).toBeFocused();
  await page.keyboard.press("Alt+ArrowUp");
  await expect(rows.filter({ hasText: firstPrompt })).toBeFocused();
  await expect(page.getByRole("button", { name: "Previous prompt" })).toBeDisabled();
  await page.getByRole("button", { name: "Next prompt" }).click();
  await expect(rows.filter({ hasText: middlePrompt })).toBeFocused();
});

test("opens a single command directly and reads a failed test", async ({ page, demo_url }, info) => {
  await openDemo(page, demo_url);
  const work = page.getByRole("button", { name: /^Worked/ });
  await work.filter({ has: page.getByText("cargo test -p parser", { exact: true }) }).click();
  const success = page.getByRole("button", { name: "Shell: cargo test -p parser", exact: true });
  await expect(success).toHaveAttribute("aria-expanded", "false");
  await success.click();
  await expect(page.getByText("test result: ok. 18 passed; 0 failed", { exact: false })).toBeVisible();

  await page.getByRole("button", { name: "Jump to prompt" }).click();
  await page.getByRole("region", { name: "Prompts in this session" }).getByRole("button").filter({ hasText: firstPrompt }).click();
  await work.filter({ hasText: "1 error" }).click();
  const failed = page.getByRole("button", { name: "Shell: cargo test -p parser --test whitespace", exact: true });
  await expect(failed).toHaveText(/exit 101/i);
  await failed.click();
  await page.getByRole("button", { name: "Show 3 more lines" }).click();
  const log = page.getByRole("region", { name: "cargo test -p parser --test whitespace" }).getByLabel(/output$/i)
    .filter({ hasText: "running 3 tests" });
  await expect(log).toContainText("assertion failed: tokens.is_empty()");
  await expect(log).toContainText("test result: FAILED. 2 passed; 1 failed");
  await log.scrollIntoViewIfNeeded();
  await capture(page, info, "failed-command");
});

test("inspects a recorded reply and returns focus to the conversation", async ({ page, demo_url }, info) => {
  await openDemo(page, demo_url);
  const availability = page.getByRole("button", { name: "Read-only session" });
  await expect(availability).toHaveAttribute("aria-expanded", "false");
  await availability.press("Enter");
  await expect(page.getByRole("button", { name: "Retry availability" })).toBeVisible();
  await availability.press("Enter");
  await expect(page.getByRole("button", { name: "Retry availability" })).toBeHidden();
  const reply = page.getByRole("article", { name: "Codex message" }).filter({ hasText: "All 18 tests pass." });
  await capture(page, info, "conversation");
  const inspect = reply.getByRole("button", { name: "Inspect assistant message" });
  await inspect.click();
  const phone = info.project.name === "phone";
  const panel = page.getByRole(phone ? "dialog" : "complementary", { name: "Event inspector" });
  await expect(panel.getByRole("button", { name: "Close inspector" })).toBeFocused();
  if (phone) await expect(panel).toHaveJSProperty("open", true);
  const content = panel.getByRole("tab", { name: "Content" });
  await expect(content).toHaveAttribute("aria-selected", "true");
  await content.press("ArrowRight");
  const normalized = panel.getByRole("tabpanel", { name: "Normalized" });
  await expect(normalized).toContainText("All 18 tests pass.");
  await expect(panel.getByRole("button", { name: "Copy normalized JSON" })).toBeVisible();
  await panel.getByRole("tab", { name: "Normalized" }).press("End");
  await expect(panel.getByRole("tabpanel", { name: "Native" })).toContainText("final_answer");
  await content.click();
  await expect(panel.getByRole("tabpanel", { name: "Content" })).toContainText("All 18 tests pass.");
  await capture(page, info, "inspector");
  await page.keyboard.press("Escape");
  await expect(panel).toBeHidden();
  await expect(inspect).toBeFocused();
});

test("reviews the recorded patch alongside its test command", async ({ page, demo_url }, info) => {
  await openDemo(page, demo_url);
  await page.getByRole("button", { name: "Jump to prompt" }).click();
  await page.getByRole("region", { name: "Prompts in this session" }).getByRole("button").filter({ hasText: middlePrompt }).click();
  await page.getByRole("button", { name: /^Worked/ }).filter({ hasText: "2 tool calls" }).click();
  await page.getByRole("button", { name: "Edit: src/lexer.rs", exact: true }).click();
  const patch = page.getByLabel("Requested changes · src/lexer.rs", { exact: true });
  await expect(patch).toContainText("-  if input.is_empty() {");
  await expect(patch).toContainText("+  if input.trim().is_empty() {");
  await expect(page.getByRole("button", { name: "Shell: cargo test -p parser --test whitespace" })).toHaveAttribute("aria-expanded", "false");
  await patch.scrollIntoViewIfNeeded();
  await capture(page, info, "file-edit");
});

test("shows a complete turn and opens session details at narrow widths", async ({ page, demo_url }, info) => {
  await openDemo(page, demo_url);
  const history = page.getByRole("region", { name: "Conversation history" });
  await history.hover();
  await page.mouse.wheel(0, -2000);
  await expect.poll(() => history.evaluate(el => el.scrollTop)).toBe(0);
  await capture(page, info, "first-turn");
  if (info.project.name === "phone") await page.setViewportSize({ width: 320, height: 700 });
  const trigger = page.getByRole("button", { name: /^Session details for / });
  await trigger.click();
  const details = page.getByRole("region", { name: "Session details" });
  await expect(details).toBeVisible();
  await expect(details.getByText("Codex", { exact: true })).toBeVisible();
  const technical = details.getByRole("checkbox", { name: "Show technical events" });
  await technical.check();
  await expect(technical).toBeChecked();
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.keyboard.press("Escape");
  await expect(trigger).toBeFocused();
  await expect(details).toBeHidden();
});
