import { expect } from "@playwright/test";
import { copyFile, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { serveFixture, test } from "./sessionFixture";

const rollout = "rollout-2026-06-02T10-00-00-3d4fa291-2faa-439d-890d-6d91f9a3342d.jsonl";
const fixture = fileURLToPath(new URL(`../../fixtures/technical-events/${rollout}`, import.meta.url));

test("keeps the transcript focused and reveals distinct usage records on demand", async ({ page, fixture_home }, info) => {
  const sessions = join(fixture_home, ".codex", "sessions", "2026", "06", "02");
  await mkdir(sessions, { recursive: true });
  await copyFile(fixture, join(sessions, rollout));

  await serveFixture(fixture_home, async url => {
    await page.goto(url, { waitUntil: "domcontentloaded" });
    await page.getByRole("main", { name: "Session workspace" }).locator(".inbox-row").click();
    const history = page.getByRole("region", { name: "Conversation history" });
    const replies = history.getByRole("article", { name: "Codex message" });
    await expect(replies).toHaveCount(2);
    await expect(replies.first()).toContainText("All eight parser tests passed.");
    await expect(replies.last()).toContainText("The suite ran with");
    const work = history.getByRole("button", { name: /^Worked for/ });
    await expect(work).toHaveCount(1);
    await expect(work).toHaveAccessibleName("Worked for <1s");
    await expect(history.getByRole("button", { name: /^Usage:/ })).toHaveCount(0);
    await page.mouse.move(0, 0);
    await page.screenshot({ path: info.outputPath(`${info.project.name}-technical-events.png`), animations: "disabled" });

    await work.click();
    const command = history.getByRole("button", { name: "Shell: make check-parser" });
    await command.click();
    await expect(history.getByText("Parser tests: 8 passed", { exact: true })).toBeVisible();
    await work.click();

    const details = page.getByRole("button", { name: /^Session details for / });
    await details.click();
    const technical = page.getByRole("checkbox", { name: "Show technical events" });
    await expect(technical).not.toBeChecked();
    await technical.check();
    await page.keyboard.press("Escape");

    const scopes = [
      { count: "270", label: "Model call · 240 input · 30 output", normalized_kind: "model_call" },
      { count: "14,580", label: "Session snapshot · 14,300 input · 280 output", normalized_kind: "session_snapshot" },
    ];
    const inspector = page.getByRole(info.project.name === "phone" ? "dialog" : "complementary", { name: "Event inspector" });
    await expect(history.getByRole("button", { name: /^Usage:/ })).toHaveCount(2);
    for (const { count, label, normalized_kind } of scopes) {
      const usage = history.getByRole("button", { name: `Usage: ${count} tokens` });
      await expect(usage).toHaveAccessibleDescription(label);
      await usage.click();
      await expect(history.getByRole("region", { name: "Usage breakdown" }).getByText(count, { exact: true })).toBeVisible();
      await usage.click();
      const inspect = history.getByRole("button", { name: `Inspect ${count} tokens` });
      await inspect.click();
      await inspector.getByRole("tab", { name: "Normalized" }).click();
      await expect(inspector.getByRole("tabpanel", { name: "Normalized" })).toContainText(`"kind": "${normalized_kind}"`);
      await page.keyboard.press("Escape");
      await expect(inspect).toBeFocused();
    }
    await expect(work).toHaveCount(1);
  });
});
