import { expect } from "@playwright/test";
import { test } from "./sessionFixture";

const original = "All 18 tests pass.";
const translated = "18 项测试全部通过。";
const idle = "Translate → 简体中文";

test("keeps translation in one action slot from the first request through toggling and retry", async ({ page, demo_url }, info) => {
  await page.addInitScript(({ original, translated }) => {
    // Exercise the viewer's browser engine without native language models or downloads.
    Object.defineProperty(globalThis, "LanguageDetector", { value: {
      availability: async () => "available",
      create: async () => ({
        detect: async () => [{ detectedLanguage: "en", confidence: 1 }],
        destroy() {},
      }),
    } });
    Object.defineProperty(globalThis, "Translator", { value: {
      availability: async () => "available",
      create: async ({ signal }: { signal: AbortSignal }) => {
        const result = new Promise<boolean>((resolve) => {
          window.addEventListener("fixture-translation-result", (event) => {
            resolve((event as CustomEvent<boolean>).detail);
          }, { once: true, signal });
        });
        return {
          async translate(text: string) {
            if (!(await result)) throw new Error("Fixture translation failed.");
            return text.replace(original, translated);
          },
          destroy() {},
        };
      },
    } });
  }, { original, translated });

  if (info.project.name === "phone") await page.setViewportSize({ width: 320, height: 844 });
  await page.goto(demo_url, { waitUntil: "domcontentloaded" });
  await page.getByRole("main", { name: "Session workspace" }).getByRole("button", { name: /Fix whitespace tokenization/ }).click();
  const reply = page.getByRole("article", { name: "Codex message" }).filter({ hasText: original });
  const action = reply.getByRole("button", { name: idle, exact: true });
  await expect(action).toHaveAttribute("title", /Simplified Chinese/);
  const element = await action.elementHandle();
  const contentId = await action.getAttribute("aria-controls");
  expect(contentId).toBeTruthy();
  // Find the same reply by the content region ID after its text changes.
  const message = page.getByRole("article", { name: "Codex message" }).filter({ has: page.locator(`[id="${contentId}"]`) });
  const content = message.locator(`[id="${contentId}"]`);

  async function inActionSlot(name: string) {
    const button = message.getByRole("button", { name, exact: true });
    await expect(button).toBeVisible();
    await expect(button).toHaveAttribute("aria-controls", contentId!);
    const slot = await button.evaluate((current, initial) => {
      const inspect = current.previousElementSibling;
      const copy = inspect?.previousElementSibling;
      const byline = current.closest("footer")?.querySelector(".message-byline");
      const position = current.getBoundingClientRect();
      const byline_position = byline?.getBoundingClientRect();
      return {
        same_element: current === initial,
        same_row: inspect?.getBoundingClientRect().top === position.top,
        after_inspect: inspect?.getAttribute("aria-label") === "Inspect assistant message"
          && position.left >= inspect.getBoundingClientRect().right,
        after_copy: copy?.getAttribute("aria-label")?.startsWith("Copy "),
        before_byline: !!byline_position && (byline_position.left >= position.right || byline_position.top >= position.bottom),
      };
    }, element);
    expect(slot).toEqual({ same_element: true, same_row: true, after_inspect: true, after_copy: true, before_byline: true });
    return button;
  }

  async function setTranslationResult(success: boolean) {
    await expect(message.getByRole("status")).toContainText("Translating response…");
    await page.evaluate((detail) => window.dispatchEvent(new CustomEvent("fixture-translation-result", { detail })), success);
  }

  const start = await inActionSlot(idle);
  if (info.project.name === "desktop") {
    await start.scrollIntoViewIfNeeded();
    await start.focus();
    await page.mouse.move(0, 0);
    await page.screenshot({ path: info.outputPath("desktop-reply-translation-idle.png"), animations: "disabled" });
  }
  await start.press("Enter");
  const pending = await inActionSlot("Translating…");
  await expect(pending).toHaveAttribute("aria-disabled", "true");
  await expect(pending).toBeFocused();
  await expect(content.getByText(original, { exact: true })).toBeVisible();
  await message.getByRole("button", { name: "Cancel", exact: true }).click();
  const cancelled = await inActionSlot(idle);
  await expect(cancelled).toBeFocused();
  await cancelled.press("Enter");
  await setTranslationResult(false);

  await expect(message.getByRole("alert")).toContainText("Fixture translation failed.");
  await (await inActionSlot("Retry translation")).press("Enter");
  await setTranslationResult(true);

  const toggle = await inActionSlot("Show original");
  await expect(toggle).toHaveAttribute("data-showing-translation", "true");
  await expect(toggle).toBeFocused();
  await expect(content).toHaveAttribute("lang", "zh-Hans");
  await expect(content.getByText(translated, { exact: true })).toBeVisible();
  await toggle.press("Enter");

  const showTranslation = await inActionSlot("Show translation");
  await expect(showTranslation).not.toHaveAttribute("data-showing-translation", "true");
  await expect(showTranslation).toBeFocused();
  await expect(content.getByText(original, { exact: true })).toBeVisible();
  await showTranslation.press("Enter");

  await expect(await inActionSlot("Show original")).toBeFocused();
  await expect(content.getByText(translated, { exact: true })).toBeVisible();
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.mouse.move(0, 0);
  await page.screenshot({ path: info.outputPath(`${info.project.name}-reply-translation.png`), animations: "disabled" });
});
