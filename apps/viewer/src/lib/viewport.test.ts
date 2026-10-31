import { afterEach, expect, it, vi } from "vitest";
import { installViewportSizing } from "./viewport";

let stop: (() => void) | undefined;
afterEach(() => { stop?.(); stop = undefined; vi.unstubAllGlobals(); });

function visualViewport() {
  const viewport = Object.assign(new EventTarget(), { height: 844, offsetTop: 0, scale: 1 });
  vi.stubGlobal("visualViewport", viewport);
  return viewport;
}
const height = () => document.documentElement.style.getPropertyValue("--app-height");
const top = () => document.documentElement.style.getPropertyValue("--app-top");

it("keeps the app above a keyboard that resizes and pans only the visual viewport", () => {
  const viewport = visualViewport();
  stop = installViewportSizing();
  expect(height()).toBe("844px");
  viewport.height = 420;
  viewport.dispatchEvent(new Event("resize"));
  expect(height()).toBe("420px");
  viewport.offsetTop = 28;
  viewport.dispatchEvent(new Event("scroll"));
  expect(top()).toBe("28px");
  viewport.height = 844; viewport.offsetTop = 0;
  viewport.dispatchEvent(new Event("resize"));
  expect(height()).toBe("844px");
  expect(top()).toBe("0px");
});

it("preserves accessible pinch zoom and removes viewport listeners on cleanup", () => {
  const viewport = visualViewport();
  stop = installViewportSizing();
  viewport.height = 422; viewport.offsetTop = 120; viewport.scale = 2;
  viewport.dispatchEvent(new Event("resize"));
  viewport.dispatchEvent(new Event("scroll"));
  expect(height()).toBe("844px");
  expect(top()).toBe("0px");
  stop(); stop = undefined;
  viewport.scale = 1;
  viewport.dispatchEvent(new Event("resize"));
  expect(height()).toBe("");
  expect(top()).toBe("");
});

it("uses window resizing when VisualViewport is unavailable", () => {
  vi.stubGlobal("visualViewport", undefined);
  vi.stubGlobal("innerHeight", 600);
  stop = installViewportSizing();
  expect(height()).toBe("600px");
  vi.stubGlobal("innerHeight", 360);
  window.dispatchEvent(new Event("resize"));
  expect(height()).toBe("360px");
});
