import { expect } from "@playwright/test";
import { copyFile, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { serveFixture, test } from "./sessionFixture";

const fixture = fileURLToPath(new URL("../../../../crates/viewer-core/tests/fixtures/codex/goal_turn.jsonl", import.meta.url));
const objective = "Make the release checklist reproducible.";

test("reads a completed Codex goal as one turn and inspects the original work", async ({ page, fixture_home }, info) => {
  const sessions = join(fixture_home, ".codex", "sessions", "2026", "06", "01");
  await mkdir(sessions, { recursive: true });
  await copyFile(fixture, join(sessions, "rollout-2026-06-01T10-00-00-2a9f7935-fd24-45ff-94b0-04d6f27a011b.jsonl"));
  await serveFixture(fixture_home, async url => {
    await page.goto(url, { waitUntil: "domcontentloaded" });
    await page.getByRole("main", { name: "Session workspace" }).locator(".inbox-row").click();
    const history = page.getByRole("region", { name: "Conversation history" });
    const goal = history.getByRole("article", { name: "User message" });
    await expect(goal.getByText(objective, { exact: true })).toBeVisible();
    await expect(goal.getByText("Sent as goal")).toBeVisible();
    const work = history.getByRole("button", { name: /^Worked for/ });
    await expect(work).toHaveCount(1);
    await expect(work).toHaveAccessibleName("Worked for 1m 38s");
    await expect(work).toHaveAttribute("aria-expanded", "false");
    const reply = history.getByRole("article", { name: "Codex message" });
    await expect(reply.getByText("Goal achieved in 1m 34s")).toBeVisible();
    await expect(history.getByText("Context compacted")).toBeHidden();
    await expect(history.getByRole("button", { name: /^Usage/ })).toHaveCount(0);
    await page.mouse.move(0, 0);
    await page.screenshot({ path: info.outputPath(`${info.project.name}-goal-turn.png`), animations: "disabled" });

    await work.click();
    const events = history.getByRole("list", { name: "Events in this turn" });
    await expect(events).toBeVisible();
    await expect(events.getByText("Context compacted")).toBeVisible();
    await expect(events.getByText(/1 failure|exit 1/).first()).toBeVisible();
    await work.click();
    await page.getByRole("button", { name: "Jump to prompt" }).click();
    await page.getByRole("region", { name: "Prompts in this session" }).getByRole("button", { name: new RegExp(objective) }).click();
    await expect(history.locator("[data-reading-prompt]")).toBeFocused();
  });
});
