import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { invoke as nativeInvoke } from "@tauri-apps/api/core";
import { openTranslationSettings } from "./tauri";
import { RemoteClient, selectMachine } from "./transport";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

beforeEach(() => { vi.resetAllMocks(); });
afterEach(() => { selectMachine(); vi.unstubAllGlobals(); });

it("opens only the viewing device's settings even while a remote machine is selected", async () => {
  vi.stubGlobal("__TAURI_INTERNALS__", {});
  const remote = new RemoteClient("https://machine.example", "unused");
  const send = vi.spyOn(remote, "invoke");
  selectMachine(remote);
  vi.mocked(nativeInvoke).mockResolvedValue(undefined);
  await openTranslationSettings();
  expect(nativeInvoke).toHaveBeenCalledExactlyOnceWith("open_translation_settings");
  expect(send).not.toHaveBeenCalled();
});

it("refuses native settings from a browser without invoking the selected host", async () => {
  const remote = new RemoteClient("https://machine.example", "unused");
  const send = vi.spyOn(remote, "invoke");
  selectMachine(remote);
  await expect(openTranslationSettings()).rejects.toThrow("macOS desktop app");
  expect(nativeInvoke).not.toHaveBeenCalled();
  expect(send).not.toHaveBeenCalled();
});
