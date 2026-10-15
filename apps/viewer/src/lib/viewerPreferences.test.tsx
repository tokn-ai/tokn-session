import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  DEFAULT_VIEWER_PREFERENCES,
  readViewerPreferences,
  resetViewerPreferences,
  updateViewerPreferences,
  useViewerPreferences,
  VIEWER_PREFERENCES_KEY,
} from "./viewerPreferences";

beforeEach(() => { resetViewerPreferences(); localStorage.clear(); readViewerPreferences(); });
afterEach(() => { cleanup(); vi.restoreAllMocks(); localStorage.clear(); });

describe("viewing device preferences", () => {
  it("has stable default snapshots and migrates existing view choices into one bounded record", () => {
    localStorage.setItem("tokn.viewer.sidebar-order", "project");
    localStorage.setItem("tokn.viewer.background-updates", "sidebar");
    localStorage.setItem("tokn.viewer.background-update-level", "final");
    localStorage.setItem("tokn.viewer.conversation-update-level", "all");
    localStorage.setItem("tokn.viewer.reading-position", "retain");
    localStorage.setItem("tokn.viewer.machines.v1", "identity stays separate");
    const migrated = readViewerPreferences();
    expect(migrated).toEqual({ version: 1, sidebar_order: "project", background_updates_scope: "sidebar",
      background_update_level: "final", conversation_update_level: "all", show_lifecycle: false });
    expect(readViewerPreferences()).toBe(migrated);
    expect(JSON.parse(localStorage.getItem(VIEWER_PREFERENCES_KEY)!)).toEqual(migrated);
    expect(localStorage.getItem("tokn.viewer.sidebar-order")).toBeNull();
    expect(localStorage.getItem("tokn.viewer.background-updates")).toBeNull();
    expect(localStorage.getItem("tokn.viewer.background-update-level")).toBeNull();
    expect(localStorage.getItem("tokn.viewer.conversation-update-level")).toBeNull();
    expect(localStorage.getItem("tokn.viewer.reading-position")).toBe("retain");
    expect(localStorage.getItem("tokn.viewer.machines.v1")).toBe("identity stays separate");
  });

  it("migrates only recognized legacy choices and keeps Details out of conversation presets", () => {
    localStorage.setItem("tokn.viewer.sidebar-order", "unknown");
    localStorage.setItem("tokn.viewer.background-updates", "unknown");
    localStorage.setItem("tokn.viewer.background-update-level", "all");
    localStorage.setItem("tokn.viewer.conversation-update-level", "details");
    expect(readViewerPreferences()).toEqual(DEFAULT_VIEWER_PREFERENCES);
  });

  it.each([
    "not json",
    JSON.stringify({ ...DEFAULT_VIEWER_PREFERENCES, version: 2 }),
    JSON.stringify({ ...DEFAULT_VIEWER_PREFERENCES, show_lifecycle: "true" }),
    JSON.stringify({ ...DEFAULT_VIEWER_PREFERENCES, conversation_update_level: "details" }),
    JSON.stringify({ version: 1 }),
    "x".repeat(4097),
  ])("recovers invalid v1 records without resurrecting legacy defaults", (saved) => {
    localStorage.setItem(VIEWER_PREFERENCES_KEY, saved);
    localStorage.setItem("tokn.viewer.sidebar-order", "project");
    expect(readViewerPreferences()).toEqual(DEFAULT_VIEWER_PREFERENCES);
  });

  it("keeps quick controls and multiple Settings consumers synchronized without remounts", () => {
    const first = renderHook(() => useViewerPreferences());
    const second = renderHook(() => useViewerPreferences());
    const initial = first.result.current.preferences;
    expect(initial).toEqual(DEFAULT_VIEWER_PREFERENCES);
    act(() => first.result.current.updatePreferences({ sidebar_order: "project", show_lifecycle: true }));
    expect(second.result.current.preferences).toBe(first.result.current.preferences);
    expect(second.result.current.preferences).toMatchObject({ sidebar_order: "project", show_lifecycle: true });
    act(() => second.result.current.updatePreferences({ background_update_level: "final" }));
    expect(first.result.current.preferences.background_update_level).toBe("final");
    first.unmount(); second.unmount();
    const reopened = renderHook(() => useViewerPreferences());
    expect(reopened.result.current.preferences).toMatchObject({ sidebar_order: "project", show_lifecycle: true, background_update_level: "final" });
  });

  it("receives bounded preference updates and resets from another window", () => {
    const { result } = renderHook(() => useViewerPreferences());
    const updated = { ...DEFAULT_VIEWER_PREFERENCES, conversation_update_level: "messages" as const };
    act(() => {
      localStorage.setItem(VIEWER_PREFERENCES_KEY, JSON.stringify(updated));
      window.dispatchEvent(new StorageEvent("storage", { key: VIEWER_PREFERENCES_KEY }));
    });
    expect(result.current.preferences).toEqual(updated);
    act(() => {
      localStorage.clear();
      window.dispatchEvent(new StorageEvent("storage", { key: null }));
    });
    expect(result.current.preferences).toEqual(DEFAULT_VIEWER_PREFERENCES);
    act(() => {
      localStorage.setItem(VIEWER_PREFERENCES_KEY, "x".repeat(4097));
      window.dispatchEvent(new StorageEvent("storage", { key: VIEWER_PREFERENCES_KEY }));
    });
    expect(result.current.preferences).toEqual(DEFAULT_VIEWER_PREFERENCES);
  });

  it("preserves in-memory edits when storage cannot be written or read", () => {
    const { result } = renderHook(() => useViewerPreferences());
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("storage full"); });
    act(() => result.current.updatePreferences({ background_updates_scope: "sidebar" }));
    expect(result.current.preferences.background_updates_scope).toBe("sidebar");
    expect(readViewerPreferences().background_updates_scope).toBe("sidebar");
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("storage unavailable"); });
    act(() => result.current.updatePreferences({ show_lifecycle: true }));
    expect(result.current.preferences).toMatchObject({ background_updates_scope: "sidebar", show_lifecycle: true });
  });

  it("keeps edits after cold-start storage reads and writes fail, then reads recover", async () => {
    vi.resetModules();
    const freshStore = await import("./viewerPreferences");
    const reads = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("storage unavailable"); });
    const writes = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("storage unavailable"); });
    freshStore.updateViewerPreferences({ show_lifecycle: true, background_update_level: "final" });
    const edited = freshStore.readViewerPreferences();
    expect(edited).toMatchObject({ show_lifecycle: true, background_update_level: "final" });
    reads.mockRestore();
    expect(freshStore.readViewerPreferences()).toBe(edited);
    expect(freshStore.readViewerPreferences()).toBe(edited);
    writes.mockRestore();
    freshStore.updateViewerPreferences({ sidebar_order: "project" });
    expect(JSON.parse(localStorage.getItem(VIEWER_PREFERENCES_KEY)!)).toMatchObject({
      show_lifecycle: true, background_update_level: "final", sidebar_order: "project",
    });
  });

  it("ignores obsolete legacy storage events while a v1 record and unsaved edits remain unchanged", () => {
    updateViewerPreferences({ sidebar_order: "project" });
    const { result } = renderHook(() => useViewerPreferences());
    const writes = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("storage full"); });
    act(() => result.current.updatePreferences({ show_lifecycle: true }));
    const edited = result.current.preferences;
    act(() => window.dispatchEvent(new StorageEvent("storage", {
      key: "tokn.viewer.sidebar-order", oldValue: "time", newValue: null,
    })));
    expect(result.current.preferences).toBe(edited);
    expect(readViewerPreferences()).toBe(edited);
    writes.mockRestore();
    act(() => {
      localStorage.removeItem(VIEWER_PREFERENCES_KEY);
      window.dispatchEvent(new StorageEvent("storage", { key: VIEWER_PREFERENCES_KEY, newValue: null }));
    });
    expect(result.current.preferences).toEqual(DEFAULT_VIEWER_PREFERENCES);
  });

  it("does not publish no-op changes and strips unrelated fields when saving", () => {
    let renders = 0;
    const { result } = renderHook(() => { renders++; return useViewerPreferences(); });
    const initial = result.current.preferences;
    act(() => updateViewerPreferences({ sidebar_order: "time" }));
    expect(result.current.preferences).toBe(initial);
    expect(renders).toBe(1);
    act(() => updateViewerPreferences({ show_lifecycle: true, device_secret: "never save" } as never));
    expect(JSON.parse(localStorage.getItem(VIEWER_PREFERENCES_KEY)!)).not.toHaveProperty("device_secret");
    act(() => updateViewerPreferences({ background_updates_scope: "unknown" } as never));
    expect(result.current.preferences.background_updates_scope).toBe("all");
  });

  it("resets viewing defaults without changing machine selections or reading positions", () => {
    localStorage.setItem("tokn.viewer.machines.v1", "machine");
    localStorage.setItem("tokn.viewer.reading-position", "position");
    updateViewerPreferences({ sidebar_order: "project", conversation_update_level: "all", show_lifecycle: true });
    resetViewerPreferences();
    expect(readViewerPreferences()).toEqual(DEFAULT_VIEWER_PREFERENCES);
    expect(localStorage.getItem("tokn.viewer.machines.v1")).toBe("machine");
    expect(localStorage.getItem("tokn.viewer.reading-position")).toBe("position");
  });

  it("repairs an invalid saved record when resetting already-default preferences", () => {
    localStorage.setItem(VIEWER_PREFERENCES_KEY, "invalid");
    expect(readViewerPreferences()).toEqual(DEFAULT_VIEWER_PREFERENCES);
    resetViewerPreferences();
    expect(JSON.parse(localStorage.getItem(VIEWER_PREFERENCES_KEY)!)).toEqual(DEFAULT_VIEWER_PREFERENCES);
  });
});
