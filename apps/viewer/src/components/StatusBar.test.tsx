import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { SessionIndexProgress, SourceError } from "../lib/types";
import { StatusBar } from "./StatusBar";

afterEach(cleanup);

function progress(overrides: Partial<SessionIndexProgress> = {}): SessionIndexProgress {
  return {
    revision: "9",
    is_refreshing: false,
    activity: "idle",
    catalog: {
      scope: "full",
      active_provider: null,
      processed_providers: 6,
      total_providers: 6,
      pending_providers: [],
      error_providers: [],
    },
    body: {
      active_provider: null,
      pending_jobs: 0,
      failed_jobs: 0,
      completed_in_run: 3,
      stale_in_run: 0,
      batch_size: 1,
      providers: [],
    },
    worker_error: null,
    retry_at_ms: null,
    ...overrides,
  };
}

function renderStatusBar(
  progressValue: SessionIndexProgress | null = progress(),
  sourceErrors: SourceError[] = [],
  onRetry = vi.fn(),
  error: string | null = null,
) {
  render(
    <>
      <button type="button">Outside</button>
      <StatusBar
        error={error}
        is_loading={false}
        is_retrying={false}
        on_retry={onRetry}
        progress={progressValue}
        source_errors={sourceErrors}
      />
    </>,
  );
  return onRetry;
}

describe("StatusBar", () => {
  it("keeps background scope in the persistent footer alongside connection and notifications", () => {
    const change = vi.fn();
    render(<StatusBar
      connection={<button type="button">This machine</button>}
      background_updates_scope="sidebar"
      background_updates_status={null}
      on_background_updates_scope_change={change}
      error={null} is_loading={false} is_retrying={false}
      on_retry={vi.fn()} progress={progress()} source_errors={[]}
    />);
    const bar = document.querySelector("footer")!;
    const trigger = screen.getByRole("button", { name: /Background updates.*Sidebar sessions/ });
    expect(bar).toContainElement(trigger);
    expect(bar).toContainElement(screen.getByRole("button", { name: "This machine" }));
    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("radio", { name: "All sessions" }));
    expect(change).toHaveBeenCalledExactlyOnceWith("all");
  });
  it("shows shared ownership while the first catalog is still pending", () => {
    const shared = progress({ activity: "waiting_for_indexer" });
    shared.catalog.pending_providers = ["codex"];
    renderStatusBar(shared);
    expect(screen.getByText("Using shared session index")).toBeTruthy();
    expect(document.querySelector(".status-bar__spinner")).toBeNull();
  });

  it("treats a pending initial catalog as active and shows every provider state", () => {
    renderStatusBar(progress({
      catalog: {
        active_provider: "codex",
        processed_providers: 1,
        total_providers: 6,
        pending_providers: ["codex", "pi"],
        error_providers: ["dsh"],
      },
      body: {
        ...progress().body,
        pending_jobs: 3,
        providers: [{
          provider: "opencode",
          pending_jobs: 3,
          failed_jobs: 0,
          completed_jobs: 0,
          total_jobs: 3,
        }],
      },
    }), [{ provider: "dsh", message: "Session log is unavailable." }]);

    expect(screen.getByText("Finding sessions · 1 / 6 providers")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /open notifications/i }));

    expect(screen.getByRole("dialog", { name: "Notifications" })).toHaveFocus();
    expect(screen.getByLabelText("Finding sessions: Looking for saved sessions.")).toBeInTheDocument();
    expect(screen.getByLabelText("Queued: Waiting to find sessions.")).toBeInTheDocument();
    expect(screen.getByLabelText("Queued · 0 / 3: Waiting to load session details.")).toBeInTheDocument();
    expect(screen.getByLabelText("Needs attention: Session log is unavailable.")).toBeInTheDocument();
    expect(screen.getAllByLabelText("Up to date: Session details are current.")).toHaveLength(2);
  });

  it("describes a targeted catalog as a check for changes", () => {
    renderStatusBar(progress({
      activity: "catalog",
      is_refreshing: true,
      catalog: {
        scope: "targeted",
        active_provider: "codex",
        processed_providers: 0,
        total_providers: 1,
        pending_providers: [],
        error_providers: [],
      },
    }));

    expect(screen.getByText("Checking for changes · 0 / 1 providers")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Checking saved sessions for changes started.");

    fireEvent.click(screen.getByRole("button", { name: /open notifications/i }));

    expect(
      screen.getByLabelText("Checking for changes: Checking saved Codex sessions for changes."),
    ).toBeInTheDocument();
  });

  it("treats a catalog payload without scope as a full discovery scan", () => {
    const legacyCatalog = { ...progress().catalog };
    delete legacyCatalog.scope;
    renderStatusBar(progress({
      activity: "catalog",
      is_refreshing: true,
      catalog: {
        ...legacyCatalog,
        active_provider: "codex",
        processed_providers: 0,
        total_providers: 1,
      },
    }));

    expect(screen.getByText("Finding sessions · 0 / 1 providers")).toBeInTheDocument();
  });

  it("shows per-provider detail progress and only marks the active provider as loading", () => {
    renderStatusBar(progress({
      activity: "body",
      is_refreshing: true,
      body: {
        ...progress().body,
        active_provider: "codex",
        pending_jobs: 219,
        providers: [
          {
            provider: "codex",
            pending_jobs: 182,
            failed_jobs: 0,
            completed_jobs: 10,
            total_jobs: 192,
          },
          {
            provider: "pi",
            pending_jobs: 37,
            failed_jobs: 0,
            completed_jobs: 0,
            total_jobs: 37,
          },
          {
            provider: "opencode",
            pending_jobs: 0,
            failed_jobs: 0,
            completed_jobs: 4,
            total_jobs: 4,
          },
        ],
      },
    }));

    fireEvent.click(screen.getByRole("button", { name: /open notifications/i }));

    expect(screen.getByText("Loading details · 10 / 192")).toBeInTheDocument();
    expect(screen.getByText("Queued · 0 / 37")).toBeInTheDocument();
    expect(screen.getByText("Up to date · 4 / 4")).toBeInTheDocument();
    expect(screen.getByText("Codex").closest("li")).toHaveAttribute("data-tone", "active");
    expect(screen.getByText("Pi").closest("li")).toHaveAttribute("data-tone", "neutral");
  });

  it("keeps an active provider's detail progress visible after an earlier job fails", () => {
    renderStatusBar(progress({
      activity: "body",
      is_refreshing: true,
      body: {
        ...progress().body,
        active_provider: "codex",
        pending_jobs: 182,
        failed_jobs: 1,
        providers: [{
          provider: "codex",
          pending_jobs: 182,
          failed_jobs: 1,
          completed_jobs: 10,
          total_jobs: 192,
        }],
      },
    }));

    fireEvent.click(screen.getByRole("button", { name: /open notifications/i }));

    const codexStatus = screen.getByLabelText(
      "Loading details · 10 / 192: A previous indexing attempt needs attention; retry indexing to try it again. Loading remaining saved session details.",
    );
    expect(codexStatus).toHaveTextContent("Loading details · 10 / 192");
    const codex = codexStatus.closest("li");
    expect(codex).toHaveAttribute("data-tone", "warning");
  });

  it("closes on Escape and outside click while restoring focus to its bell", () => {
    renderStatusBar();
    const bell = screen.getByRole("button", { name: /open notifications/i });
    fireEvent.click(bell);
    expect(screen.getByRole("dialog", { name: "Notifications" })).toBeInTheDocument();

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Notifications" })).not.toBeInTheDocument();
    expect(bell).toHaveFocus();

    fireEvent.click(bell);
    fireEvent.pointerDown(screen.getByRole("button", { name: "Outside" }));
    expect(screen.queryByRole("dialog", { name: "Notifications" })).not.toBeInTheDocument();
    expect(bell).toHaveFocus();
  });

  it("lets keyboard focus leave a non-modal status panel without pulling it back", () => {
    renderStatusBar();
    fireEvent.click(screen.getByRole("button", { name: /open notifications/i }));
    const outside = screen.getByRole("button", { name: "Outside" });
    act(() => outside.focus());
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(outside).toHaveFocus();
  });

  it("offers the real retry action without a notification history", () => {
    const onRetry = renderStatusBar(progress({
      activity: "waiting_to_retry",
      body: {
        ...progress().body,
        pending_jobs: 2,
      },
    }));

    expect(screen.getByText("Details queued · 2 remaining")).toBeInTheDocument();
    expect(document.querySelector(".status-bar__spinner")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /open notifications/i }));
    fireEvent.click(screen.getByRole("button", { name: "Retry indexing" }));

    expect(onRetry).toHaveBeenCalledOnce();
    expect(screen.queryByText("Previous notifications")).not.toBeInTheDocument();
  });

  it("shows a generic worker failure without exposing scheduler error text", () => {
    renderStatusBar(progress({
      activity: "waiting_to_retry",
      worker_error: "task_failed",
    }));

    expect(screen.getByText("Session index worker needs attention · retry scheduled")).toBeInTheDocument();
    expect(document.querySelector(".status-bar__spinner")).not.toBeInTheDocument();
    const bell = screen.getByRole("button", { name: /open notifications/i });
    expect(bell).toHaveAttribute("data-has-attention", "true");
    fireEvent.click(bell);

    expect(screen.getByText("Session index worker")).toBeInTheDocument();
    expect(screen.getByText(
      "The background session index task stopped unexpectedly. Retry indexing to start another pass.",
    )).toBeInTheDocument();
    expect(screen.queryByText(/stack trace|\/Users\//i)).not.toBeInTheDocument();
  });

  it("keeps a failed retry visible even when a last-known index snapshot exists", () => {
    renderStatusBar(progress(), [], vi.fn(), "Session index scheduler is unavailable.");

    expect(screen.getByText("Session index needs attention")).toBeInTheDocument();
    expect(screen.queryByText("Session index is up to date")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /open notifications/i }));
    expect(screen.getByText("Session index control")).toBeInTheDocument();
    expect(screen.getAllByText("Session index scheduler is unavailable.")).toHaveLength(2);
  });

  it("keeps changing counts out of its polite phase announcement", () => {
    const first = progress({
      activity: "body",
      is_refreshing: true,
      body: {
        ...progress().body,
        pending_jobs: 4,
      },
    });
    const { container, rerender } = render(
      <StatusBar
        error={null}
        is_loading={false}
        is_retrying={false}
        on_retry={vi.fn()}
        progress={first}
        source_errors={[]}
      />,
    );

    expect(container.querySelector(".status-bar__summary")).not.toHaveAttribute("aria-live");
    expect(screen.getByRole("status")).toHaveTextContent("Loading session details started.");
    expect(screen.getByRole("status")).not.toHaveTextContent("4");

    rerender(
      <StatusBar
        error={null}
        is_loading={false}
        is_retrying={false}
        on_retry={vi.fn()}
        progress={{
          ...first,
          revision: "10",
          body: { ...first.body, pending_jobs: 3 },
        }}
        source_errors={[]}
      />,
    );
    expect(screen.getByRole("status")).toHaveTextContent("Loading session details started.");
    expect(screen.getByRole("status")).not.toHaveTextContent("3");

    rerender(
      <StatusBar
        error={null}
        is_loading={false}
        is_retrying={false}
        on_retry={vi.fn()}
        progress={{
          ...first,
          revision: "11",
          body: { ...first.body, failed_jobs: 1 },
        }}
        source_errors={[]}
      />,
    );
    expect(screen.getByRole("status")).toHaveTextContent("Session index needs attention.");
  });
});
