import { useCallback, useRef, useState, type ReactNode, type RefObject } from "react";
import type { BackgroundUpdateLevel, BackgroundUpdatesScope, ConversationUpdateLevel, SessionIndexProgress, SourceError } from "../lib/types";
import { BackgroundUpdatesControl } from "./BackgroundUpdatesControl";
import { BellIcon, SettingsIcon } from "./Icons";
import {
  describeSessionIndexProgress,
  isTargetedCatalog,
  NotificationCenter,
} from "./NotificationCenter";

interface StatusBarProps {
  on_open_settings?: () => void;
  settings_trigger_ref?: RefObject<HTMLButtonElement | null>;
  background_updates_scope?: BackgroundUpdatesScope;
  background_updates_level?: BackgroundUpdateLevel;
  conversation_update_level?: ConversationUpdateLevel;
  conversation_available?: boolean;
  background_updates_status?: string | null;
  on_background_updates_scope_change?: (scope: BackgroundUpdatesScope) => void;
  on_background_updates_level_change?: (level: BackgroundUpdateLevel) => void;
  on_conversation_update_level_change?: (level: ConversationUpdateLevel) => void;
  connection?: ReactNode;
  progress: SessionIndexProgress | null;
  is_loading: boolean;
  error: string | null;
  source_errors: SourceError[];
  is_retrying: boolean;
  on_retry: () => void;
}

interface StatusAnnouncement {
  key: string;
  text: string;
}

/**
 * Deliberately exclude counters from live text. The scheduler can update
 * queue sizes many times during one phase, and repeating those updates would
 * turn a useful status bar into screen-reader noise.
 */
function describeStatusAnnouncement(
  progress: SessionIndexProgress | null,
  isLoading: boolean,
  error: string | null,
  hasAttention: boolean,
): StatusAnnouncement {
  if (hasAttention) {
    return { key: "needs-attention", text: "Session index needs attention." };
  }
  if (!progress) {
    return isLoading
      ? { key: "checking", text: "Checking session index." }
      : {
        key: error ? "unavailable-warning" : "unavailable",
        text: "Session index status is unavailable.",
      };
  }
  if (progress.activity === "waiting_for_indexer") {
    return { key: "shared-index", text: "Reading the shared session index. Another viewer process owns indexing." };
  }
  if (progress.catalog.pending_providers.length > 0 || progress.activity === "catalog") {
    if (isTargetedCatalog(progress)) {
      return { key: "checking-changes", text: "Checking saved sessions for changes started." };
    }
    return { key: "finding-sessions", text: "Finding saved sessions started." };
  }
  if (progress.body.pending_jobs > 0 || progress.activity === "body") {
    return progress.activity === "waiting_to_retry"
      ? {
        key: "details-waiting",
        text: "Session details are queued for the next batch.",
      }
      : { key: "details", text: "Loading session details started." };
  }
  if (progress.activity === "waiting_to_retry") {
    return { key: "refresh-waiting", text: "Session index refresh scheduled." };
  }
  return { key: "up-to-date", text: "Session index is up to date." };
}

/**
 * The persistent, low-noise entry point to the current indexing operation.
 * It intentionally owns only popover visibility; progress state remains in
 * the viewer hook so a background event cannot be lost while it is closed.
 */
export function StatusBar({
  on_open_settings,
  settings_trigger_ref,
  background_updates_scope = "all",
  background_updates_level = "messages",
  conversation_update_level = "steps",
  conversation_available = false,
  background_updates_status = null,
  on_background_updates_scope_change,
  on_background_updates_level_change,
  on_conversation_update_level_change,
  connection,
  progress,
  is_loading,
  error,
  source_errors,
  is_retrying,
  on_retry,
}: StatusBarProps) {
  const [notificationsOpen, setNotificationsOpen] = useState(false);
  const notificationTriggerRef = useRef<HTMLButtonElement | null>(null);
  const summary = describeSessionIndexProgress(progress, is_loading, error, source_errors);
  // A queued retry is visible in the summary, but does not spin as if its
  // provider were currently being read. The active-provider fields cover the
  // small transition between scheduler phases where is_refreshing is not yet
  // reflected in a received snapshot.
  const hasActiveProvider = (progress?.catalog.active_provider ?? null) !== null
    || (progress?.body.active_provider ?? null) !== null;
  const isActive = (progress?.is_refreshing ?? false) || is_loading || hasActiveProvider;
  const hasAttention = summary.tone === "warning";
  const announcement = describeStatusAnnouncement(progress, is_loading, error, hasAttention);
  const closeNotifications = useCallback(() => setNotificationsOpen(false), []);
  const toggleNotifications = useCallback(() => setNotificationsOpen((open) => !open), []);

  return (
    <div className="status-region">
      <footer className="status-bar" data-tone={summary.tone}>
        {connection}
        {on_background_updates_scope_change && on_background_updates_level_change && on_conversation_update_level_change ? <BackgroundUpdatesControl
          scope={background_updates_scope}
          level={background_updates_level}
          conversation_level={conversation_update_level}
          conversation_available={conversation_available}
          status={background_updates_status}
          on_scope_change={on_background_updates_scope_change}
          on_level_change={on_background_updates_level_change}
          on_conversation_level_change={on_conversation_update_level_change}
        /> : null}

        {on_open_settings && <button aria-label="Open settings" aria-haspopup="dialog"
          className="status-bar__settings" onClick={on_open_settings} ref={settings_trigger_ref}
          title="Settings" type="button"><SettingsIcon /></button>}

        <span aria-atomic="true" aria-live="polite" className="sr-only" key={announcement.key} role="status">
          {announcement.text}
        </span>

        <button
          aria-controls="notification-center"
          aria-expanded={notificationsOpen}
          aria-haspopup="dialog"
          aria-label={`${summary.label}. Open notifications`}
          className="status-bar__index"
          data-has-attention={hasAttention}
          onClick={toggleNotifications}
          ref={notificationTriggerRef}
          title={summary.label}
          type="button"
        >
          {isActive ? <span aria-hidden="true" className="status-bar__spinner" /> : <BellIcon />}
          <span className="status-bar__summary">{summary.label}</span>
          {hasAttention ? <span aria-hidden="true" className="status-bar__notification-dot" /> : null}
        </button>
      </footer>

      <NotificationCenter
        error={error}
        is_loading={is_loading}
        is_open={notificationsOpen}
        is_retrying={is_retrying}
        on_close={closeNotifications}
        on_retry={on_retry}
        progress={progress}
        source_errors={source_errors}
        trigger_ref={notificationTriggerRef}
      />
    </div>
  );
}
