import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { browserTranslationEngine } from "../lib/browserTranslation";
import { getTranslationStatus } from "../lib/tauri";
import { viewerCapabilities } from "../lib/platform";
import type { TranslationStatus } from "../lib/types";
import { TranslationProvider, useTranslationStatus } from "./TranslationProvider";

vi.mock("../lib/tauri", () => ({ getTranslationStatus: vi.fn() }));
vi.mock("../lib/platform", async (original) => ({ ...await original<typeof import("../lib/platform")>(), viewerCapabilities: vi.fn() }));
vi.mock("../lib/browserTranslation", () => ({
  browserTranslationEngine: {
    label: "Browser Translation",
    description: "Translate locally in your browser.",
    getStatus: vi.fn(),
    start: vi.fn(),
  },
}));

function Status({ label = "status" }: { label?: string }) {
  const status = useTranslationStatus();
  return <output aria-label={label}>{status === null ? "Unchecked" : status.available ? "Available" : status.reason}</output>;
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(viewerCapabilities).mockReturnValue({ native_translation: true } as ReturnType<typeof viewerCapabilities>);
  vi.mocked(getTranslationStatus).mockResolvedValue({ available: true, reason: null });
  vi.mocked(browserTranslationEngine.getStatus).mockResolvedValue({ available: true, reason: null });
});
afterEach(cleanup);

describe("TranslationProvider", () => {
  it("shares one desktop availability check across consumers and rerenders", async () => {
    const view = render(<TranslationProvider><Status label="first" /><Status label="second" /></TranslationProvider>);
    expect(await screen.findAllByText("Available")).toHaveLength(2);
    expect(getTranslationStatus).toHaveBeenCalledTimes(1);
    expect(browserTranslationEngine.getStatus).not.toHaveBeenCalled();
    view.rerender(<TranslationProvider><Status label="replacement" /></TranslationProvider>);
    expect(screen.getByLabelText("replacement")).toHaveTextContent("Available");
    expect(getTranslationStatus).toHaveBeenCalledTimes(1);
  });

  it("shares one browser availability check without invoking native commands or preparing models", async () => {
    vi.mocked(viewerCapabilities).mockReturnValue({ native_translation: false } as ReturnType<typeof viewerCapabilities>);
    const view = render(<TranslationProvider><Status label="first" /><Status label="second" /></TranslationProvider>);
    expect(await screen.findAllByText("Available")).toHaveLength(2);
    view.rerender(<TranslationProvider><Status /></TranslationProvider>);
    expect(screen.getByLabelText("status")).toHaveTextContent("Available");
    expect(browserTranslationEngine.getStatus).toHaveBeenCalledTimes(1);
    expect(browserTranslationEngine.start).not.toHaveBeenCalled();
    expect(getTranslationStatus).not.toHaveBeenCalled();
  });

  it("provides the browser's reason when local translation is unavailable", async () => {
    vi.mocked(viewerCapabilities).mockReturnValue({ native_translation: false } as ReturnType<typeof viewerCapabilities>);
    vi.mocked(browserTranslationEngine.getStatus).mockResolvedValue({ available: false, reason: "This browser does not support local translation." });
    render(<TranslationProvider><Status /></TranslationProvider>);
    expect(await screen.findByText("This browser does not support local translation.")).toBeInTheDocument();
  });

  it("provides the native reason when translation is unavailable", async () => {
    vi.mocked(getTranslationStatus).mockResolvedValue({ available: false, reason: "Requires macOS 15 or later." });
    render(<TranslationProvider><Status /></TranslationProvider>);
    expect(await screen.findByText("Requires macOS 15 or later.")).toBeInTheDocument();
  });

  it("converts availability-check failures into a readable unavailable state", async () => {
    vi.mocked(getTranslationStatus).mockRejectedValue(new Error("Missing native command"));
    render(<TranslationProvider><Status /></TranslationProvider>);
    expect(await screen.findByText("Apple Translation is unavailable in this viewer.")).toBeInTheDocument();
  });

  it("labels browser availability failures without referring to Apple Translation", async () => {
    vi.mocked(viewerCapabilities).mockReturnValue({ native_translation: false } as ReturnType<typeof viewerCapabilities>);
    vi.mocked(browserTranslationEngine.getStatus).mockRejectedValue(new Error("Browser policy blocked translation."));
    render(<TranslationProvider><Status /></TranslationProvider>);
    expect(await screen.findByText("Browser Translation is unavailable in this viewer.")).toBeInTheDocument();
    expect(getTranslationStatus).not.toHaveBeenCalled();
  });

  it.each(["desktop", "browser"])("ignores a late %s availability result from an unmounted provider", async (platform) => {
    vi.mocked(viewerCapabilities).mockReturnValue({ native_translation: platform === "desktop" } as ReturnType<typeof viewerCapabilities>);
    const get_status = platform === "desktop" ? getTranslationStatus : browserTranslationEngine.getStatus;
    let resolve!: (status: TranslationStatus) => void;
    vi.mocked(get_status).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
    const old = render(<TranslationProvider><Status /></TranslationProvider>);
    expect(screen.getByLabelText("status")).toHaveTextContent("Unchecked");
    old.unmount();
    vi.mocked(get_status).mockResolvedValue({ available: false, reason: "Unavailable in this viewer." });
    render(<TranslationProvider><Status /></TranslationProvider>);
    await screen.findByText("Unavailable in this viewer.");
    await act(async () => resolve({ available: true, reason: null }));
    expect(screen.getByLabelText("status")).toHaveTextContent("Unavailable in this viewer.");
  });
});
