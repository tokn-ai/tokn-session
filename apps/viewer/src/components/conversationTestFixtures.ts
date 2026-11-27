import type { ComponentProps } from "react";
import { vi } from "vitest";
import type { EventSummary, SessionSummary } from "../lib/types";
import type { Conversation } from "./Conversation";

export const SESSION: SessionSummary = {
  session_key: "codex:root", session_id: "root", parent_session_id: null,
  is_subagent: false, provider: "codex", title: "Session title", preview: null,
  project: null, cwd: null, updated_at_ms: null, timestamp: null,
  agent_path: null, agent_nickname: null, agent_role: null, child_count: 0,
  message_count: null, event_count: null, history_status: null, has_unread: false,
};

export function event(key: string, overrides: Partial<EventSummary> = {}): EventSummary {
  return {
    event_key: key, type: "lifecycle", provider: "codex", timestamp: null,
    phase: null, role: null, title: key, summary: `${key} detail`,
    summary_truncated: false, is_hidden: false, is_error: false,
    tool: null, usage: null, reasoning: null, is_bookkeeping: true,
    ...overrides,
  };
}

export function props(overrides: Partial<ComponentProps<typeof Conversation>> = {}): ComponentProps<typeof Conversation> {
  return {
    session: SESSION, events: [], selected_event_key: null, expanded_event_key: null,
    expanded_detail: null, expanded_detail_error: null, expanded_detail_loading: false,
    initial_page_loaded: true, is_loading: false, is_loading_older: false,
    is_loading_newer: false, error: null, has_older: false, has_newer: false,
    total_events: 10, history_status: null, inspector_open: false,
    on_sidebar_open: vi.fn(), on_inspector_toggle: vi.fn(), on_event_select: vi.fn(),
    on_event_toggle: vi.fn(), trajectory_pages: new Map(), trajectory_expanded_key: null,
    trajectory_expanded_event_key: null, trajectory_expanded_detail: null,
    trajectory_expanded_detail_error: null, trajectory_expanded_detail_loading: false,
    on_trajectory_retry: vi.fn(), on_trajectory_event_toggle: vi.fn(),
    on_trajectory_retry_expanded_detail: vi.fn(), on_open_related_session: vi.fn(),
    on_load_older: vi.fn(), on_load_newer: vi.fn(), on_retry: vi.fn(),
    on_retry_expanded_detail: vi.fn(), on_follow_change: vi.fn(),
    ...overrides,
  };
}
