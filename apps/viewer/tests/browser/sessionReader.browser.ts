import { expect, type Page } from "@playwright/test";
import { promptEnd, replyEnd, test, title } from "./sessionFixture";

async function openSession(page: Page, url: string) {
  await page.goto(url, { waitUntil: "domcontentloaded" });
  await page.getByRole("main", { name: "Session workspace" }).getByRole("button", { name: new RegExp(title) }).click();
}

test("reads a clipped transcript and retries an interrupted detail request", async ({ page, viewer }) => {
  const errors: Error[] = [];
  page.on("pageerror", error => errors.push(error));
  let detailRequests = 0;
  await page.route("**/api/v1/load_event_detail", async route => {
    detailRequests++;
    if (detailRequests === 1) await route.fulfill({ status: 503, body: "Unavailable" });
    else await route.continue();
  });

  await openSession(page, viewer.url);
  const read = page.getByRole("button", { name: "Read full prompt" });
  await expect(read).toBeVisible();
  expect(detailRequests).toBe(0);

  await read.focus();
  await page.keyboard.press("Enter");
  const retry = page.getByRole("button", { name: "Retry full message" });
  await expect(retry).toBeFocused();
  await expect(retry).toBeInViewport();
  await page.keyboard.press("Enter");
  await expect(page.getByText(promptEnd, { exact: false })).toBeVisible();
  const collapse = page.getByRole("button", { name: "Show less" });
  await expect(collapse).toBeFocused();
  await collapse.click();
  await expect(page.getByText(promptEnd, { exact: false })).toHaveCount(0);

  await page.getByRole("button", { name: "Read full reply" }).click();
  await expect(page.getByText(replyEnd, { exact: false })).toBeVisible();
  expect(detailRequests).toBe(3);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(page.viewportSize()!.width);
  expect(errors).toEqual([]);
});

test("keeps the reading position when a new turn arrives", async ({ page, viewer }) => {
  await openSession(page, viewer.url);
  await page.getByRole("button", { name: "Read full prompt" }).click();
  await expect(page.getByText(promptEnd, { exact: false })).toBeVisible();
  await page.getByRole("button", { name: "Jump to prompt" }).click();
  await page.getByRole("region", { name: "Prompts in this session" }).getByRole("button", { name: /Review empty inputs/ }).click();
  const prompt = page.locator("[data-reading-prompt]").first();
  await expect(prompt).toBeInViewport();
  const top = await prompt.evaluate(el => el.getBoundingClientRect().top);

  const reply = "The new parser report is ready.";
  await viewer.appendTurn("Run the parser again.", reply);
  const incoming = page.getByLabel("Session event timeline").getByText(reply, { exact: true });
  await expect(incoming).toBeAttached({ timeout: 15_000 });
  await expect(incoming).not.toBeInViewport();
  expect(Math.abs(await prompt.evaluate(el => el.getBoundingClientRect().top) - top)).toBeLessThanOrEqual(2);
  await page.getByRole("button", { name: /Jump to latest/ }).click();
  await expect(incoming).toBeInViewport();
});
