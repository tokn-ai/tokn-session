import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { SessionSummary, ViewerProvider } from "../lib/types";
import { Sidebar } from "./Sidebar";

afterEach(cleanup);

function session(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    session_key: "codex:session",
    session_id: "01991dce-7f6a-7000-8000-000000000001",
    parent_session_id: null,
    is_subagent: false,
    provider: "codex",
    title: null,
    preview: null,
    project: "Viewer",
    cwd: "/work/viewer",
    updated_at_ms: null,
    timestamp: null,
    agent_path: null,
    agent_nickname: null,
    agent_role: null,
    child_count: 0,
    message_count: null,
    event_count: null,
    history_status: null,
    has_unread: false,
    ...overrides,
  };
}

function renderSidebar(
  sessions: SessionSummary[],
  pendingProviders: ViewerProvider[] = [],
  error: string | null = null,
  onCollapse?: () => void,
) {
  return render(
    <Sidebar
      enabled_providers={new Set(["codex"])}
      error={error}
      has_more={false}
      is_loading={false}
      is_loading_more={false}
      on_children_load={vi.fn()}
      on_collapse={onCollapse}
      on_children_load_more={vi.fn()}
      on_children_retry={vi.fn()}
      on_load_more={vi.fn()}
      on_provider_toggle={vi.fn()}
      on_retry={vi.fn()}
      on_search_change={vi.fn()}
      on_session_select={vi.fn()}
      search=""
      session_children={new Map()}
      selected_session_key={null}
      sessions={sessions}
      source_errors={[]}
      pending_providers={pendingProviders}
    />,
  );
}

describe("Sidebar session identity", () => {
  it("keeps provider controls in a disclosure and delegates desktop collapse", () => {
    const collapse = vi.fn();
    renderSidebar([session()], [], null, collapse);
    const disclosure = screen.getByText("Providers").closest("details");
    expect(disclosure).not.toHaveAttribute("open");
    fireEvent.click(screen.getByText("Providers"));
    expect(disclosure).toHaveAttribute("open");
    expect(screen.getByRole("button", { name: "Codex" })).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByRole("button", { name: "Collapse sessions" }));
    expect(collapse).toHaveBeenCalledOnce();
  });
  it("keeps async question attention visible beside running state and unread replies", () => {
    const { rerender } = renderSidebar([session({ is_running: true, question_attention: { required_count: 0, available_count: 2 } })]);
    expect(screen.getByRole("img", { name: "Question available, 2 unanswered questions" })).toBeInTheDocument();
    expect(screen.getByRole("img", { name: "Running" })).toBeInTheDocument();
    rerender(<Sidebar enabled_providers={new Set(["codex"])} error={null} has_more={false} is_loading={false} is_loading_more={false}
      on_children_load={vi.fn()} on_children_load_more={vi.fn()} on_children_retry={vi.fn()} on_load_more={vi.fn()}
      on_provider_toggle={vi.fn()} on_retry={vi.fn()} on_search_change={vi.fn()} on_session_select={vi.fn()}
      search="" session_children={new Map()} selected_session_key={null} sessions={[session({
        is_running: true, has_unread: true, unread_final_count: 1, question_attention: { required_count: 1, available_count: 0 },
      })]} source_errors={[]} pending_providers={[]} />);
    expect(screen.getByRole("img", { name: "Input required, 1 unanswered question" })).toBeInTheDocument();
    expect(screen.getByRole("img", { name: "1 unread final reply" })).toBeInTheDocument();
    expect(screen.queryByRole("img", { name: "Running" })).not.toBeInTheDocument();
  });
  it("offers WorkBuddy as a provider filter", () => {
    renderSidebar([]);

    const filter = screen.getByRole("button", { name: "WorkBuddy" });
    expect(filter).toHaveAttribute("data-provider", "workbuddy");
    expect(filter).toHaveAttribute("aria-pressed", "false");
  });

  it("shows an explicit cold-index state instead of treating it as an empty catalog", () => {
    renderSidebar([], ["codex"]);

    expect(screen.getByText("Building session catalog")).toBeInTheDocument();
    expect(screen.getByText("Indexing Codex. Known sessions will appear here shortly."))
      .toBeInTheDocument();
    expect(screen.queryByText("No sessions found")).not.toBeInTheDocument();
  });

  it("keeps indexed rows visible when a catalog refresh fails", () => {
    renderSidebar([session({ title: "Indexed session" })], [], "local index is temporarily unavailable");

    expect(screen.getByRole("alert")).toHaveTextContent("Session catalog could not refresh");
    expect(screen.getByRole("alert")).toHaveTextContent("local index is temporarily unavailable");
    expect(screen.getByText("Indexed session")).toBeInTheDocument();
  });

  it("renders title fallbacks while keeping the full session id discoverable", () => {
    const titledId = "01991dce-7f6a-7000-8000-000000000001";
    const previewId = "abcdef01-2345-6789-abcd-ef0123456789";
    const untitledId = "12345678-90ab-cdef-1234-567890abcdef";

    renderSidebar([
      session({
        session_key: "codex:titled",
        session_id: titledId,
        title: "Provider title",
        preview: "First prompt should not win",
      }),
      session({
        session_key: "codex:preview",
        session_id: previewId,
        preview: "First user prompt",
      }),
      session({
        session_key: "codex:untitled",
        session_id: untitledId,
        title: "  ",
        preview: "\n\t",
      }),
    ]);

    const titled = screen.getByRole("button", {
      name: `Provider title, Codex session ${titledId}`,
    });
    expect(within(titled).getByText("Provider title")).toHaveClass("session-row__title");
    expect(within(titled).getByText("First prompt should not win")).toHaveClass("session-row__preview");
    expect(within(titled).getByText("Codex")).toHaveClass("session-row__provider");
    expect(within(titled).queryByText(titledId)).not.toBeInTheDocument();
    expect(titled).toHaveAttribute("title", `Provider title\nCodex · Viewer\n${titledId}`);

    const preview = screen.getByRole("button", {
      name: `First user prompt, Codex session ${previewId}`,
    });
    expect(within(preview).getByText("First user prompt")).toHaveClass("session-row__title");
    expect(within(preview).getByText("Codex")).toHaveClass("session-row__provider");
    expect(preview).toHaveAttribute("title", `First user prompt\nCodex · Viewer\n${previewId}`);

    const untitled = screen.getByRole("button", {
      name: `Untitled session, Codex session ${untitledId}`,
    });
    expect(within(untitled).getByText("Untitled session")).toHaveClass("session-row__title");
    expect(within(untitled).getByText("Codex")).toHaveClass("session-row__provider");
    expect(untitled).toHaveAttribute("title", `Untitled session\nCodex · Viewer\n${untitledId}`);
  });

  it("loads missing child metadata only after an expansion commits", async () => {
    const onChildrenLoad = vi.fn();
    const parent = session({
      session_key: "codex:parent",
      session_id: "parent-0000",
      title: "Root task",
      child_count: 1,
    });

    render(
      <Sidebar
        enabled_providers={new Set(["codex"])}
        error={null}
        has_more={false}
        is_loading={false}
        is_loading_more={false}
        on_children_load={onChildrenLoad}
        on_children_load_more={vi.fn()}
        on_children_retry={vi.fn()}
        on_load_more={vi.fn()}
        on_provider_toggle={vi.fn()}
        on_retry={vi.fn()}
        on_search_change={vi.fn()}
        on_session_select={vi.fn()}
        search=""
        session_children={new Map()}
        selected_session_key={null}
        sessions={[parent]}
        source_errors={[]}
        pending_providers={[]}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Show 1 subagent for Root task" }));
    await waitFor(() => expect(onChildrenLoad).toHaveBeenCalledWith(parent.session_key));
  });

  it("renders cached nested subagents as independently selectable tree items", () => {
    const onChildrenLoad = vi.fn();
    const onSessionSelect = vi.fn();
    const parent = session({
      session_key: "codex:parent",
      session_id: "parent-0000",
      title: "Root task",
      child_count: 1,
    });
    const child = session({
      session_key: "codex:child",
      session_id: "child-0000",
      parent_session_id: "parent-0000",
      is_subagent: true,
      title: null,
      agent_nickname: "Hubble",
      agent_path: "/root/researcher",
    });

    render(
      <Sidebar
        enabled_providers={new Set(["codex"])}
        error={null}
        has_more={false}
        is_loading={false}
        is_loading_more={false}
        on_children_load={onChildrenLoad}
        on_children_load_more={vi.fn()}
        on_children_retry={vi.fn()}
        on_load_more={vi.fn()}
        on_provider_toggle={vi.fn()}
        on_retry={vi.fn()}
        on_search_change={vi.fn()}
        on_session_select={onSessionSelect}
        search=""
        session_children={new Map([[
          parent.session_key,
          {
            sessions: [child],
            next_cursor: null,
            is_loading: false,
            is_loading_more: false,
            error: null,
          },
        ]])}
        selected_session_key={null}
        sessions={[parent]}
        source_errors={[]}
        pending_providers={[]}
      />,
    );

    expect(screen.queryByRole("button", { name: /subagent Hubble/i })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Show 1 subagent for Root task" }));
    expect(onChildrenLoad).not.toHaveBeenCalled();

    const childRow = screen.getByRole("button", {
      name: `subagent Hubble, Codex session ${child.session_id}`,
    });
    expect(childRow).toHaveTextContent("/root/researcher");
    fireEvent.click(childRow);
    expect(onSessionSelect).toHaveBeenCalledWith(child.session_key);
  });
});

describe("Sidebar unread activity", () => {
  it("shows only a running circle, then the accumulated final-reply count, then nothing", () => {
    const running = session({ is_running: true, has_unread: true, unread_final_count: 3 });
    renderSidebar([running]);
    expect(screen.getByRole("img", { name: "Running" })).toHaveClass("inline-spinner");
    expect(screen.queryByRole("img", { name: /unread/ })).not.toBeInTheDocument();
    cleanup();
    renderSidebar([{ ...running, is_running: false }]);
    expect(screen.getByRole("img", { name: "3 unread final replies" })).toHaveTextContent("3");
    expect(screen.queryByRole("img", { name: "Running" })).not.toBeInTheDocument();
    cleanup();
    renderSidebar([{ ...running, is_running: false, has_unread: false, unread_final_count: 0 }]);
    expect(screen.queryByRole("img")).not.toBeInTheDocument();
  });

  it("prioritizes a running descendant over an unread parent", () => {
    renderSidebar([session({ has_running_descendant: true, has_unread: true, unread_final_count: 2 })]);
    expect(screen.getByRole("img", { name: "Subagent running" })).toBeInTheDocument();
    expect(screen.queryByRole("img", { name: /unread/ })).not.toBeInTheDocument();
  });

  it("renders an accessible indicator for a directly unread session", () => {
    const unread = session({
      session_key: "codex:unread",
      session_id: "unread-0000",
      title: "Needs attention",
      has_unread: true,
    });

    renderSidebar([unread]);

    const row = screen.getByRole("button", {
      name: "Needs attention, Codex session unread-0000, 1 unread final reply",
    });
    const dot = within(row).getByRole("img", { name: "1 unread final reply" });

    expect(row).toHaveAttribute("data-unread", "true");
    expect(dot).toHaveClass("session-row__unread-dot");
    expect(dot).toHaveAttribute("data-unread-source", "direct");
  });

  it("counts only the parent’s own replies even when an older server reports descendant attention", () => {
    renderSidebar([session({ title: "Parent", has_unread: true, unread_final_count: 1,
      has_unread_descendant: true, unread_descendant_count: 2 })]);
    expect(screen.getByRole("img", { name: "1 unread final reply" })).toHaveClass("session-row__unread-dot");
    expect(screen.queryByRole("img", { name: /3 unread/ })).not.toBeInTheDocument();
  });

  it("ignores legacy descendant counts on a parent while showing the child’s own unread replies", () => {
    const parent = session({
      session_key: "codex:parent",
      session_id: "parent-0000",
      title: "Root task",
      child_count: 1,
      has_unread_descendant: true,
      unread_descendant_count: 2,
    });
    const child = session({
      session_key: "codex:child",
      session_id: "child-0000",
      parent_session_id: parent.session_id,
      is_subagent: true,
      agent_nickname: "Hubble",
      has_unread: true,
    });

    render(
      <Sidebar
        enabled_providers={new Set(["codex"])}
        error={null}
        has_more={false}
        is_loading={false}
        is_loading_more={false}
        on_children_load={vi.fn()}
        on_children_load_more={vi.fn()}
        on_children_retry={vi.fn()}
        on_load_more={vi.fn()}
        on_provider_toggle={vi.fn()}
        on_retry={vi.fn()}
        on_search_change={vi.fn()}
        on_session_select={vi.fn()}
        search=""
        session_children={new Map([[
          parent.session_key,
          {
            sessions: [child],
            next_cursor: null,
            is_loading: false,
            is_loading_more: false,
            error: null,
          },
        ]])}
        selected_session_key={null}
        sessions={[parent]}
        source_errors={[]}
        pending_providers={[]}
      />,
    );

    const parentRow = screen.getByRole("button", {
      name: "Root task, Codex session parent-0000",
    });
    expect(parentRow).not.toHaveAttribute("data-unread");
    expect(within(parentRow).queryByRole("img", { name: /unread/ })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Show 1 subagent for Root task" }));

    const childRow = screen.getByRole("button", {
      name: "subagent Hubble, Codex session child-0000, 1 unread final reply",
    });
    expect(within(childRow).getByRole("img", { name: "1 unread final reply" }))
      .toHaveAttribute("data-unread-source", "direct");
  });
});
