import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import type { EventDetail, EventSummary } from "../lib/types";
import { formatTimestamp, providerLabel, readableEventContent } from "../lib/state";
import { CloseIcon, WarningIcon } from "./Icons";
import { MarkdownContent } from "./MarkdownContent";
import { CopyTextButton } from "./CopyTextButton";
import { ActionNotificationHost } from "./ActionNotifications";
import { useViewerVisible } from "../lib/viewerVisibility";
import "./Inspector.css";

interface InspectorProps {
  is_open: boolean;
  event: EventSummary | null;
  detail: EventDetail | null;
  is_loading: boolean;
  error: string | null;
  on_close: () => void;
  on_retry: () => void;
}

type InspectorTab = "content" | "normalized" | "native";
const TABS = { content: "Content", normalized: "Normalized", native: "Native" } as const;
const TAB_ORDER: InspectorTab[] = ["content", "normalized", "native"];
const COMPACT_PANEL = "(max-width: 860px)";

export function Inspector({
  is_open,
  event,
  detail: loaded_detail,
  is_loading,
  error: loadError,
  on_close,
  on_retry,
}: InspectorProps) {
  const visible = useViewerVisible();
  const open = is_open && visible;
  const [compact, setCompact] = useState(() => window.matchMedia(COMPACT_PANEL).matches);
  const [preferredTab, setPreferredTab] = useState<InspectorTab | null>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const panelId = useId();
  const matchesDetail = loaded_detail?.event_key === event?.event_key
    && (!event?.content_revision || loaded_detail?.content_revision === event.content_revision);
  const detail = matchesDetail ? loaded_detail : null;
  const error = loadError ?? (loaded_detail && !matchesDetail ? "The event changed. Reload its details." : null);

  useEffect(() => {
    const media = window.matchMedia(COMPACT_PANEL);
    const change = () => setCompact(media.matches);
    media.addEventListener("change", change);
    change();
    return () => media.removeEventListener("change", change);
  }, []);

  useLayoutEffect(() => {
    if (!open) return;
    const dialog = dialogRef.current;
    if (compact) dialog?.showModal();
    closeButtonRef.current?.focus({ preventScroll: true });
    return () => { if (dialog?.open) dialog.close(); };
  }, [compact, open]);

  const readableContent = useMemo(
    () => event ? readableEventContent(event, detail) : null,
    [detail, event],
  );
  const isRedactedReasoning = event?.type === "reasoning" && event.reasoning?.is_redacted === true;
  const contentAvailable = readableContent !== null;
  const nativeAvailable = !!detail && detail.native !== null && !detail.is_hidden && !event?.is_hidden && !isRedactedReasoning;
  const availableTabs = TAB_ORDER.filter((tab) =>
    tab === "normalized" || (tab === "content" ? contentAvailable : nativeAvailable));
  const activeTab = preferredTab && availableTabs.includes(preferredTab)
    ? preferredTab : contentAvailable ? "content" : "normalized";
  useLayoutEffect(() => {
    const panel = contentRef.current;
    if (panel) {
      panel.scrollTop = 0;
      panel.scrollLeft = 0;
    }
  }, [event?.event_key, activeTab, open]);

  const redactedNormalizedEvent = event && (isRedactedReasoning || event.is_hidden || detail?.is_hidden) ? {
    type: event.type,
    provider: event.provider,
    redacted: true,
  } : null;
  const visibleValue = activeTab === "native"
    ? isRedactedReasoning ? null : detail?.native
    : activeTab === "normalized"
      ? redactedNormalizedEvent ?? detail?.event
      : undefined;
  const formattedValue = useMemo(() => {
    if (!open || !detail || visibleValue === undefined) {
      return "";
    }
    return JSON.stringify(visibleValue, null, 2);
  }, [open, detail, visibleValue]);

  function onTabKeyDown(keyboardEvent: KeyboardEvent<HTMLButtonElement>) {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(keyboardEvent.key)) {
      return;
    }
    keyboardEvent.preventDefault();
    const currentIndex = availableTabs.indexOf(keyboardEvent.currentTarget.value as InspectorTab);
    const nextIndex = keyboardEvent.key === "Home"
      ? 0
      : keyboardEvent.key === "End"
        ? availableTabs.length - 1
        : keyboardEvent.key === "ArrowRight"
          ? (currentIndex + 1) % availableTabs.length
          : (currentIndex - 1 + availableTabs.length) % availableTabs.length;
    const nextTab = availableTabs[nextIndex]!;
    setPreferredTab(nextTab);
    document.getElementById(`${panelId}-${nextTab}`)?.focus({ preventScroll: true });
  }

  const contents = <>
    <header className="inspector__header" data-tauri-drag-region>
      <div>
        <p className="eyebrow">Event inspector</p>
        <h2 title={event?.title}>{event?.title ?? "Event detail"}</h2>
      </div>
      <button
        aria-label="Close inspector"
        className="icon-button"
        onClick={on_close}
        ref={closeButtonRef}
        type="button"
      >
        <CloseIcon />
      </button>
    </header>

    {!event ? (
      <div className="inspector__empty">
        <span className="empty-orbit" aria-hidden="true" />
        <strong>No event selected</strong>
        <p>Select any message or technical event to inspect its normalized shape.</p>
      </div>
    ) : (
      <>
        <dl className="event-facts">
          <div>
            <dt>Provider</dt>
            <dd>
              <span className="provider-badge" data-provider={event.provider}>
                {providerLabel(event.provider)}
              </span>
            </dd>
          </div>
          <div>
            <dt>Event</dt>
            <dd>{event.type.replace(/_/g, " ")}</dd>
          </div>
          <div>
            <dt>Time</dt>
            <dd>{formatTimestamp(event.timestamp)}</dd>
          </div>
          {event.phase ? (
            <div>
              <dt>Phase</dt>
              <dd>{event.phase}</dd>
            </div>
          ) : null}
        </dl>

        {detail?.is_hidden || event.is_hidden ? (
          <div className="redaction-notice" role="status">
            <WarningIcon />
            <span>
              <strong>Content hidden by provider</strong>
              <small>The viewer does not request or render its native payload.</small>
            </span>
          </div>
        ) : null}

        {isRedactedReasoning ? (
          <div className="redaction-notice" role="status">
            <WarningIcon />
            <span>
              <strong>Reasoning redacted by provider</strong>
              <small>The viewer does not render its readable or native payload.</small>
            </span>
          </div>
        ) : null}

        {event.summary_truncated
          && detail
          && !readableContent
          && !detail.is_hidden
          && !event.is_hidden
          && !isRedactedReasoning ? (
            <div className="content-notice" role="status">
              The timeline preview was capped, but this event has no readable plain-text field.
              Normalized and native data remain available below.
            </div>
          ) : null}

        <div className="inspector__tools">
          <div className="inspector-tabs" role="tablist" aria-label="Event representation">
            {TAB_ORDER.filter((tab) => tab !== "content" || contentAvailable).map((tab) => (
              <button key={tab} aria-controls={panelId} aria-selected={activeTab === tab}
                className="inspector-tab" disabled={!availableTabs.includes(tab)} id={`${panelId}-${tab}`}
                onKeyDown={onTabKeyDown} onClick={() => setPreferredTab(tab)} role="tab"
                tabIndex={activeTab === tab ? 0 : -1} value={tab} type="button">{TABS[tab]}</button>
            ))}
          </div>
          {formattedValue && <div className="inspector__copy">
            <CopyTextButton key={`${event.event_key}:${activeTab}`} text={formattedValue}
              label={activeTab === "native" ? "Copy native JSON" : "Copy normalized JSON"} class_name="inspector__copy-button" />
          </div>}
        </div>

        <div
          aria-busy={is_loading}
          aria-labelledby={`${panelId}-${activeTab}`}
          className="inspector__content"
          id={panelId}
          ref={contentRef}
          role="tabpanel"
          tabIndex={0}
        >
          {is_loading && !detail ? (
            <div className="detail-loading" role="status">
              <span className="spinner" />
              Loading event detail…
            </div>
          ) : null}
          {!is_loading && error ? (
            <div className={detail ? "detail-refresh-error" : "detail-error"} role="alert">
              <strong>{detail ? "Could not refresh detail" : "Detail unavailable"}</strong>
              <span>{error}</span>
              <button className="text-button" onClick={on_retry} type="button">
                Try again
              </button>
            </div>
          ) : null}
          {detail ? (
            activeTab === "content" && readableContent ? (
              <div className="readable-content">
                {readableContent.sections.map((section, index) => (
                  <section key={`${section.label ?? "content"}-${index}`}>
                    {section.label ? <h3>{section.label}</h3> : null}
                    <MarkdownContent
                      class_name="readable-content__text"
                      content={section.text}
                    />
                  </section>
                ))}
              </div>
            ) : visibleValue === null ? (
              <div className="detail-empty">No native payload is available for this event.</div>
            ) : visibleValue !== undefined ? (
              <pre>{formattedValue}</pre>
            ) : null
          ) : null}
        </div>
      </>
    )}
  </>;

  if (compact) return <dialog ref={dialogRef} aria-label="Event inspector" className="inspector inspector--modal"
    data-open={open} onCancel={(cancel) => { cancel.preventDefault(); on_close(); }}
    onClick={(click) => {
      if (click.target !== click.currentTarget) return;
      const bounds = click.currentTarget.getBoundingClientRect();
      if (click.clientX < bounds.left || click.clientX > bounds.right || click.clientY < bounds.top || click.clientY > bounds.bottom) on_close();
    }}>
    {open && <>{contents}<ActionNotificationHost /></>}
  </dialog>;
  return <aside aria-label="Event inspector" aria-hidden={!open} inert={!open} className="inspector" data-open={open}>
    {open && contents}
  </aside>;
}
