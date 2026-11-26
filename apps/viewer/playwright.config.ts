import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/browser",
  testMatch: "**/*.browser.ts",
  use: { browserName: "firefox" },
  projects: [
    { name: "desktop", use: { viewport: { width: 1366, height: 900 }, colorScheme: "dark" } },
    { name: "phone", use: { viewport: { width: 390, height: 844 }, hasTouch: true, colorScheme: "light" } },
  ],
});
