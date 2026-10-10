import { SessionDisplayCache } from "./sessionDisplayCache";
import type { ExpandedActivityState } from "./types";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { loadReadingWindow, readReadingPosition, readingEventKey } from "./readingPosition";
import { refreshEventWindow, loadCompleteTrajectory } from "./liveEvents";
import { compareProjects, readSessionOrder, saveSessionOrder } from "./sidebarOrder";
import { useSessionView } from "./useSessionView";
import {
  acknowledgeSessionAttention,
  getSessionIndexProgress,
  listSessionChildren,
  listSessions,
  listenForSessionIndexChanges,
  listenForSessionIndexProgress,
  listenForRelayChanges,
  listenForTransportReconnect,
  listenForSessionUpdates,
  listenForSessionNotifications,
  loadSessionUpdates,
  loadEventDetail,
  loadEventPage,
  loadTrajectoryEventPage,
  retrySessionIndex as requestSessionIndexRetry,
} from "./tauri";
import {
  EVENT_PAGE_SIZE,
  SESSION_PAGE_SIZE,
  errorMessage,
  eventButtonId,
  findKnownSession,
  mergeEvents,
  mergeSessions,
  preserveEventSelection,
  preserveSessionSelection,
} from "./state";
import {
  PROVIDERS,
  type EventDetail,
  type EventSummary,
  type OutstandingQuestion,
  type SessionChildrenState,
  type SessionHistoryStatus,
  type SessionIndexProgress,
  type SessionSummary,
  type SessionOrder,
  type SourceError,
  type TrajectoryEventPageResponse,
  type TrajectoryEventPageState,
  type ViewerProvider,
} from "./types";

function useDebouncedValue<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timeout = window.setTimeout(() => setDebounced(value), delayMs);
    return () => window.clearTimeout(timeout);
  }, [delayMs, value]);
  return debounced;
}

const DETAIL_CACHE_LIMIT = 50;
// Legacy transport chunk size; it never limits a displayed activity group.
const TRAJECTORY_TRANSPORT_PAGE_SIZE = 40;
const INPUT_REFRESH_DELAYS_MS = [1_000, 3_000, 8_000, 20_000];

function compareDecimalRevisions(left: string, right: string): number {
  const normalizedLeft = left.replace(/^0+(?=\d)/, "");
  const normalizedRight = right.replace(/^0+(?=\d)/, "");
  if (normalizedLeft.length !== normalizedRight.length) {
    return normalizedLeft.length < normalizedRight.length ? -1 : 1;
  }
  if (normalizedLeft === normalizedRight) {
    return 0;
  }
  return normalizedLeft < normalizedRight ? -1 : 1;
}

function readCachedDetail(cache: Map<string, EventDetail>, key: string): EventDetail | null {
  const detail = cache.get(key) ?? null;
  if (detail) {
    cache.delete(key);
    cache.set(key, detail);
  }
  return detail;
}

function writeCachedDetail(cache: Map<string, EventDetail>, key: string, detail: EventDetail) {
  cache.delete(key);
  cache.set(key, detail);
  while (cache.size > DETAIL_CACHE_LIMIT) {
    const oldestKey = cache.keys().next().value as string | undefined;
    if (!oldestKey) {
      break;
    }
    cache.delete(oldestKey);
  }
}

function expandedEventNeedsDetail(event: EventSummary | null | undefined): boolean {
  if (!event || event.is_hidden) {
    return false;
  }
  if (event.type === "tool_call" || event.type === "question_request" || event.type === "question_reply") {
    return true;
  }
  if (event.type === "compaction") {
    return event.compaction?.has_summary === true;
  }
  if (event.type === "agent_activity") {
    return event.agent_activity?.communication?.has_text === true;
  }
  return event.type === "reasoning"
    && event.reasoning !== null
    && !event.reasoning.is_redacted
    && (event.reasoning.has_summary || event.reasoning.has_text);
}

function emptyTrajectoryEventPageState(): TrajectoryEventPageState {
  return {
    events: [],
    total_events: null,
    has_loaded: false,
    is_loading: false,
    error: null,
  };
}

function trajectoryRequestKey(sessionKey: string, trajectoryKey: string): string {
  return `${sessionKey}\u0000${trajectoryKey}`;
}

interface ExpandedTrajectoryEvent {
  trajectory_key: string;
  event_key: string;
}

/**
 * A newest-page response that has been accepted by the request-generation
 * guard. It is kept in React state so the acknowledgement effect runs only
 * after the same render commits the corresponding timeline page.
 */
interface AcceptedInitialEventPage {
  sessionKey: string;
  requestId: number;
  attentionRevision: string;
}

export function useViewerState() {
  const [sessionOrder, setSessionOrder] = useState(readSessionOrder);
  const [search, setSearchValue] = useState("");
  const debouncedSearch = useDebouncedValue(search.trim(), 180);
  const [enabledProviders, setEnabledProviders] = useState<Set<ViewerProvider>>(
    () => new Set(PROVIDERS),
  );
  const providerKey = PROVIDERS.filter((provider) => enabledProviders.has(provider)).join(",");
  const sessionQueryKey = `${sessionOrder}\u0000${providerKey}\u0000${debouncedSearch}`;

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [sessionChildren, setSessionChildren] = useState<Map<string, SessionChildrenState>>(
    () => new Map(),
  );
  const sessionChildrenRef = useRef<Map<string, SessionChildrenState>>(new Map());
  const sessionChildrenGeneration = useRef(0);
  const sessionChildRequests = useRef(new Map<string, number>());
  const [selectedSessionKey, setSelectedSessionKey] = useState<string | null>(null);
  const selectedSessionKeyRef = useRef<string | null>(null);
  // A verified communication sender can be outside the loaded sidebar pages.
  // Keep its selected metadata without asserting any new parent/child edge.
  const [selectedSessionMetadata, setSelectedSessionMetadata] = useState<SessionSummary | null>(null);
  const [sessionsLoading, setSessionsLoading] = useState(true);
  const [sessionsLoadingMore, setSessionsLoadingMore] = useState(false);
  const [sessionsError, setSessionsError] = useState<string | null>(null);
  const [sourceErrors, setSourceErrors] = useState<SourceError[]>([]);
  const [pendingProviders, setPendingProviders] = useState<ViewerProvider[]>([]);
  const [sessionsCursor, setSessionsCursor] = useState<string | null>(null);
  const [sessionsAttempt, setSessionsAttempt] = useState(0);
  const sessionsRequest = useRef(0);
  const sessionListInFlight = useRef<number | null>(null);
  const sessionListRefreshQueued = useRef(false);
  const previousSessionFilterKey = useRef<string | null>(null);
  const previousSessionQueryKey = useRef<string | null>(null);
  const [sessionIndexListenerReady, setSessionIndexListenerReady] = useState(false);
  const [sessionIndexProgress, setSessionIndexProgress] = useState<SessionIndexProgress | null>(null);
  const [sessionIndexProgressLoading, setSessionIndexProgressLoading] = useState(true);
  const [sessionIndexProgressError, setSessionIndexProgressError] = useState<string | null>(null);
  const [sessionIndexRetrying, setSessionIndexRetrying] = useState(false);
  const sessionIndexProgressRevision = useRef<string | null>(null);
  const sessionIndexConnectionEpoch = useRef(0);
  const sessionIndexRetryInFlight = useRef(false);

  const finishSessionListRequest = useCallback((requestId: number) => {
    if (sessionListInFlight.current !== requestId) return;
    sessionListInFlight.current = null;
    if (sessionListRefreshQueued.current) {
      sessionListRefreshQueued.current = false;
      setSessionsAttempt((attempt) => attempt + 1);
    }
  }, []);

  useEffect(() => () => {
    // Switching machines unmounts this hook. An old request must not start a
    // queued catalog read against the newly selected transport.
    sessionsRequest.current += 1;
    sessionListInFlight.current = null;
    sessionListRefreshQueued.current = false;
  }, []);

  const [events, setEvents] = useState<EventSummary[]>([]);
  const [outstandingQuestions, setOutstandingQuestions] = useState<OutstandingQuestion[]>([]);
  const [pageQuestionAttention, setPageQuestionAttention] = useState<SessionSummary["question_attention"]>(undefined);
  const [questionNavigation, setQuestionNavigation] = useState<{ event_key: string; revision: number } | null>(null);
  const pendingQuestionSession = useRef<string | null>(null);
  const [eventsOwnerKey, setEventsOwnerKey] = useState<string | null>(null);
  const eventsOwnerKeyRef = useRef<string | null>(null);
  const [initialPageSessionKey, setInitialPageSessionKey] = useState<string | null>(null);
  const [selectedEventKey, setSelectedEventKey] = useState<string | null>(null);
  const selectedEventKeyRef = useRef<string | null>(null);
  const [eventsLoading, setEventsLoading] = useState(false);
  const [olderLoading, setOlderLoading] = useState(false);
  const [newerLoading, setNewerLoading] = useState(false);
  const [eventsError, setEventsError] = useState<string | null>(null);
  const [followError, setFollowError] = useState<string | null>(null);
  const [olderCursor, setOlderCursor] = useState<string | null>(null);
  const [newerCursor, setNewerCursor] = useState<string | null>(null);
  const [totalEvents, setTotalEvents] = useState<number | null>(null);
  const [historyStatus, setHistoryStatus] = useState<SessionHistoryStatus | null>(null);
  const [eventsAttempt, setEventsAttempt] = useState(0);
  const eventsRequest = useRef(0);
  const followingLive = useRef(true);
  const [isFollowingLive, setIsFollowingLive] = useState(true);
  const pendingLiveReset = useRef(false);
  const uncommittedLiveReset = useRef(false);
  const liveRefresh = useRef(false);
  const eventRefreshInFlight = useRef(false);
  const liveUpdateQueued = useRef(false);
  const inputRefreshTimers = useRef<number[]>([]);
  const eventsRef = useRef<EventSummary[]>([]);
  eventsRef.current = events;
  const [pendingLiveActivity, setPendingLiveActivity] = useState(false);
  const [acceptedInitialEventPage, setAcceptedInitialEventPage] = useState<
    AcceptedInitialEventPage | null
  >(null);
  const acknowledgedInitialPageRequest = useRef<number | null>(null);
  const [trajectoryPages, setTrajectoryPages] = useState<
    Map<string, Map<string, TrajectoryEventPageState>>
  >(() => new Map());
  const trajectoryPagesRef = useRef<Map<string, Map<string, TrajectoryEventPageState>>>(
    new Map(),
  );
  const trajectoryPageGeneration = useRef(0);
  const trajectoryPageRequests = useRef(new Map<string, number>());

  const [inspectorOpen, setInspectorOpen] = useState(false);
  const inspectorTriggerRef = useRef<HTMLElement | null>(null);
  const [mobileSidebarOpen, setMobileSidebarOpen] = useState(false);
  const [detail, setDetail] = useState<EventDetail | null>(null);
  const [detailOwnerKey, setDetailOwnerKey] = useState<string | null>(null);
  const detailOwnerKeyRef = useRef<string | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [detailAttempt, setDetailAttempt] = useState(0);
  const detailRequest = useRef(0);
  const detailCache = useRef(new Map<string, EventDetail>());
  const displayCache = useRef(new SessionDisplayCache());
  const pushedPage = useRef<import("./types").EventPageResponse | null>(null);
  const semanticLive = useRef(new Set<string>());
  const notificationsReady = useRef(false);
  const semanticSupported = useRef<boolean | null>(null);
  const onSessionUpdate = useRef<(update: import("./types").SessionUpdate) => void>(() => {});
  const loadWorkPage = useCallback(async (request: import("./types").LoadTrajectoryEventPageRequest) => {
    const cached = displayCache.current.trajectoryGroup(request.session_key, request.trajectory_key);
    if (cached) return cached;
    if (request.trajectory_key.startsWith("activity:")) {
      displayCache.current.includeGroup(request.session_key, request.trajectory_key);
      const update = await loadSessionUpdates(displayCache.current.request(request.session_key, "steps"));
      if (!displayCache.current.apply(update)) throw new Error("Work group changed; try again");
      const group = displayCache.current.trajectoryGroup(request.session_key, request.trajectory_key);
      if (!group) throw new Error("Work group is no longer available");
      return group;
    }
    return loadTrajectoryEventPage(request);
  }, []);
  const loadDisplayPage = useCallback(async (request: import("./types").LoadEventPageRequest) => {
    if (semanticSupported.current === false) return loadEventPage(request);
    if (request.window_mode !== "retained") {
      displayCache.current.includeHistory(request.session_key);
    }
    let update: import("./types").SessionUpdate;
    try { update = await loadSessionUpdates({ ...displayCache.current.request(request.session_key, "steps"),
        ...(request.window_mode === "earlier" ? { history_cursor: request.cursor } : {}) }); }
    catch (error: unknown) {
      if (/unknown.*command|unknown.*variant|command.*not found|unavailable for a session share/i.test(errorMessage(error))) {
        semanticSupported.current = false;
        return loadEventPage(request);
      }
      throw error;
    }
    const page = displayCache.current.apply(update);
    if (!page) throw new Error("Session updates changed; retry the snapshot");
    semanticSupported.current = true;
    semanticLive.current.add(request.session_key);
    return page;
  }, []);

  const detailLoads = useRef(new Map<string, Promise<EventDetail>>());
  const detailGeneration = useRef(0);
  const [detailRevision, setDetailRevision] = useState(0);
  const [expandedEventKey, setExpandedEventKey] = useState<string | null>(null);
  const expandedEventKeyRef = useRef<string | null>(null);
  const manualExpansion = useRef(false);
  const expansionRevision = useRef(0);
  const workingTrajectory = useRef<string | null>(null);
  const [expandedDetail, setExpandedDetail] = useState<EventDetail | null>(null);
  const [expandedDetailOwnerKey, setExpandedDetailOwnerKey] = useState<string | null>(null);
  const expandedDetailOwnerKeyRef = useRef<string | null>(null);
  const [expandedDetailLoading, setExpandedDetailLoading] = useState(false);
  const [expandedDetailError, setExpandedDetailError] = useState<string | null>(null);
  const [expandedDetailAttempt, setExpandedDetailAttempt] = useState(0);
  const expandedDetailRequest = useRef(0);
  const [expandedTrajectoryEvent, setExpandedTrajectoryEvent] = useState<
    ExpandedTrajectoryEvent | null
  >(null);
  const [expandedActivityKeys, setExpandedActivityKeys] = useState(new Set<string>());
  const [expandedActivities, setExpandedActivities] = useState(new Map<string, ExpandedActivityState>());
  const [expandedTrajectoryDetailAttempt, setExpandedTrajectoryDetailAttempt] = useState(0);
  const expandedTrajectoryDetailRequest = useRef(0);
  const sessionDisclosures = useRef(new Map<string, { expanded_event_key: string | null; selected_event_key: string | null; inspector_open: boolean; manual_expansion: boolean }>());
  const [visibleActivityGroups, setVisibleActivityGroups] = useState(new Set<string>());
  const setActivityGroupVisibility = useCallback((groupKey: string, visible: boolean) => {
    setVisibleActivityGroups((current) => {
      if (current.has(groupKey) === visible) return current;
      const next = new Set(current);
      if (visible) next.add(groupKey); else next.delete(groupKey);
      return next;
    });
  }, []);
  const activeDetailKeys = useRef<string[]>([]);
  activeDetailKeys.current = [...new Set([
    ...(inspectorOpen && selectedEventKey ? [selectedEventKey] : []),
    ...(expandedEventKey && expandedEventNeedsDetail(events.find((event) => event.event_key === expandedEventKey)) ? [expandedEventKey] : []),
    ...[...expandedActivityKeys].filter((key) => {
      if (!selectedSessionKey || !expandedEventKey) return false;
      const group = displayCache.current.activityGroupForEvent(selectedSessionKey, key);
      if (group) return visibleActivityGroups.has(group);
      return trajectoryPagesRef.current.get(selectedSessionKey)?.get(expandedEventKey)?.events.some((event) => event.event_key === key);
    }),
  ])].slice(0, 16);


  const applyExpandedEventKey = useCallback((key: string | null) => {
    expandedEventKeyRef.current = key;
    setExpandedEventKey(key);
  }, []);

  const applySessionIndexProgress = useCallback(
    (next: SessionIndexProgress, source: "event" | "snapshot" | "retry") => {
      const currentRevision = sessionIndexProgressRevision.current;
      if (currentRevision !== null) {
        const comparison = compareDecimalRevisions(next.revision, currentRevision);
        // The event listener is established before the first snapshot. If an
        // event arrives while that snapshot is still loading, retain the
        // event at the same or newer revision rather than regressing to the
        // snapshot that was captured earlier.
        if (comparison < 0 || (comparison === 0 && source === "snapshot")) {
          return false;
        }
      }
      sessionIndexProgressRevision.current = next.revision;
      setSessionIndexProgress(next);
      setSessionIndexProgressError(null);
      return true;
    },
    [],
  );

  useEffect(() => {
    let disposed = false;
    let unlistenProgress: (() => void) | undefined;
    let unlistenReconnect: (() => void) | undefined;

    async function readSnapshot(epoch: number) {
      try {
        const progress = await getSessionIndexProgress();
        if (!disposed && epoch === sessionIndexConnectionEpoch.current) {
          applySessionIndexProgress(progress, "snapshot");
        }
      } catch (error: unknown) {
        if (!disposed && epoch === sessionIndexConnectionEpoch.current) {
          setSessionIndexProgressError(errorMessage(error));
        }
      } finally {
        if (!disposed && epoch === sessionIndexConnectionEpoch.current) {
          setSessionIndexProgressLoading(false);
        }
      }
    }

    // Register both listeners before awaiting readiness so the reconnect
    // signal cannot race the first progress snapshot.
    const progressSubscription = listenForSessionIndexProgress((progress) => {
      if (!disposed) {
        applySessionIndexProgress(progress, "event");
      }
    });
    const reconnectSubscription = listenForTransportReconnect(() => {
      if (disposed) return;
      const epoch = ++sessionIndexConnectionEpoch.current;
      // A restarted API can begin its progress revisions at one again.
      sessionIndexProgressRevision.current = null;
      setSessionIndexProgressLoading(true);
      void readSnapshot(epoch);
    });

    async function subscribeThenReadSnapshot() {
      try {
        unlistenProgress = await progressSubscription;
      } catch {
        // The static Vite preview and browser-based component tests do not
        // have Tauri's event bridge. The command snapshot below can still
        // populate this surface when a caller provides one.
      }
      try {
        unlistenReconnect = await reconnectSubscription;
      } catch {
        // A remote reconnect signal is advisory; the first snapshot still works.
      }

      if (disposed) {
        unlistenProgress?.();
        unlistenReconnect?.();
        return;
      }
      await readSnapshot(sessionIndexConnectionEpoch.current);
    }

    void subscribeThenReadSnapshot();
    return () => {
      disposed = true;
      unlistenProgress?.();
      unlistenReconnect?.();
    };
  }, [applySessionIndexProgress]);

  const retrySessionIndex = useCallback(async () => {
    if (sessionIndexRetryInFlight.current) {
      return;
    }
    sessionIndexRetryInFlight.current = true;
    setSessionIndexRetrying(true);
    const epoch = sessionIndexConnectionEpoch.current;
    try {
      const progress = await requestSessionIndexRetry();
      if (epoch === sessionIndexConnectionEpoch.current) {
        applySessionIndexProgress(progress, "retry");
      }
    } catch (error: unknown) {
      if (epoch === sessionIndexConnectionEpoch.current) {
        setSessionIndexProgressError(errorMessage(error));
      }
    } finally {
      sessionIndexRetryInFlight.current = false;
      setSessionIndexRetrying(false);
    }
  }, [applySessionIndexProgress]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;

    void listenForSessionIndexChanges((change) => {
      if (change.catalog_refresh_required !== false || !notificationsReady.current) setSessionsAttempt((attempt) => attempt + 1);
      const selectedSessionKey = selectedSessionKeyRef.current;
      if (selectedSessionKey && !semanticLive.current.has(selectedSessionKey) && (
        change.updated_session_keys?.includes(selectedSessionKey)
        || change.attention_session_keys.includes(selectedSessionKey)
      )) {
        // Body refresh is separate from unread attention: a tool/lifecycle
        // update must reach the selected page without creating a dot.
        // Older backends still send only attention_session_keys.
        liveRefresh.current = true;
        setPendingLiveActivity(!followingLive.current);
        if (eventRefreshInFlight.current) liveUpdateQueued.current = true;
        else setEventsAttempt((attempt) => attempt + 1);
      }
    })
      .then((stop) => {
        if (disposed) {
          stop();
          return;
        }
        unlisten = stop;
        // Do not begin the first catalog read until its change subscription is
        // live. Otherwise an index commit between the read and registration
        // can leave an initially empty sidebar stale until the next refresh.
        setSessionIndexListenerReady(true);
      })
      // Browser-based tests and the static Vite preview do not expose Tauri's
      // event bridge. Listings continue to work without background refreshes.
      .catch(() => {
        if (!disposed) {
          setSessionIndexListenerReady(true);
        }
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const acknowledgeAcceptedAttention = useCallback((sessionKey: string, attentionRevision: string | null | undefined) => {
    if (!attentionRevision) {
      return;
    }
    void acknowledgeSessionAttention({
      session_key: sessionKey,
      attention_revision: attentionRevision,
    })
      .then((response) => {
        if (!response.changed) {
          return;
        }
        // Refresh the sidebar only after SQLite committed the seen cursor.
        // A failed acknowledgement must leave the visible dot intact.
        setSessionsAttempt((attempt) => attempt + 1);
      })
      .catch(() => {});
  }, []);

  const requestDetail = useCallback((sessionKey: string, eventKey: string) => {
    const cacheKey = `${sessionKey}:${eventKey}`;
    const pending = detailLoads.current.get(cacheKey);
    if (pending) {
      return pending;
    }
    const generation = detailGeneration.current;
    let request: Promise<EventDetail>;
    const load = async () => {
      const delivered = displayCache.current.detail(sessionKey, eventKey);
      if (delivered) return delivered;
      if (!semanticLive.current.has(sessionKey)) return loadEventDetail({ session_key: sessionKey, event_key: eventKey });
      const keys = [...new Set([...activeDetailKeys.current, eventKey])].slice(-16);
      const update = await loadSessionUpdates(displayCache.current.request(sessionKey, "details", keys));
      displayCache.current.apply(update);
      const detail = displayCache.current.detail(sessionKey, eventKey);
      if (!detail) throw new Error("Tool details changed; retry loading them");
      return detail;
    };
    request = load().then((response) => {
      if (detailGeneration.current === generation) {
        writeCachedDetail(detailCache.current, cacheKey, response);
      }
      return response;
    }).finally(() => {
      if (detailLoads.current.get(cacheKey) === request) {
        detailLoads.current.delete(cacheKey);
      }
    });
    detailLoads.current.set(cacheKey, request);
    return request;
  }, []);

  const invalidateEventDetails = useCallback((retainVisible: boolean) => {
    detailGeneration.current += 1;
    const owner = selectedSessionKeyRef.current;
    for (const key of detailCache.current.keys()) if (!owner || key.startsWith(`${owner}:`)) detailCache.current.delete(key);
    detailLoads.current.clear();
    detailRequest.current += 1;
    expandedDetailRequest.current += 1;
    expandedTrajectoryDetailRequest.current += 1;
    setDetailError(null);
    setExpandedDetailError(null);
    // Appends retain event identities. Keep already displayed content while
    // the cache and requests refresh, so detail cards do not shrink to loaders.
    // Replacement generations can reuse source positions for different events.
    if (!retainVisible) {
      detailOwnerKeyRef.current = null;
      setDetailOwnerKey(null);
      setDetail(null);
      setDetailLoading(false);
      expandedDetailOwnerKeyRef.current = null;
      setExpandedDetailOwnerKey(null);
      setExpandedDetail(null);
      setExpandedDetailLoading(false);
      setExpandedActivities(new Map());
    }
    setDetailRevision((revision) => revision + 1);
  }, []);

  const updateTrajectoryPage = useCallback(
    (
      sessionKey: string,
      trajectoryKey: string,
      update: (current: TrajectoryEventPageState | undefined) => TrajectoryEventPageState,
    ) => {
      const next = new Map(trajectoryPagesRef.current);
      const sessionPages = new Map(next.get(sessionKey) ?? []);
      sessionPages.set(trajectoryKey, update(sessionPages.get(trajectoryKey)));
      next.set(sessionKey, sessionPages);
      trajectoryPagesRef.current = next;
      setTrajectoryPages(next);
    },
    [],
  );

  const clearTrajectoryPages = useCallback(() => {
    trajectoryPageGeneration.current += 1;
    trajectoryPageRequests.current.clear();
    const next = new Map<string, Map<string, TrajectoryEventPageState>>();
    trajectoryPagesRef.current = next;
    setTrajectoryPages(next);
    expandedTrajectoryDetailRequest.current += 1;
    setExpandedTrajectoryEvent(null);
    setExpandedActivityKeys(new Set());
    setExpandedActivities(new Map());
  }, []);

  const invalidateTrajectoryPages = useCallback((reload: boolean, changed?: ReadonlySet<string>) => {
    trajectoryPageGeneration.current += 1;
    trajectoryPageRequests.current.clear();
    const retained = new Map([...trajectoryPagesRef.current].map(([sessionKey, pages]) => [
      sessionKey,
      new Map([...pages].map(([key, page]) => [key, {
        ...page,
        has_loaded: reload && (!changed || changed.has(key) || page.events.some((event) => changed.has(event.event_key))) ? false : page.has_loaded,
        is_loading: false,
      }])),
    ]));
    // Cancelled requests cannot clear their own loading flags. Keep rows usable
    // if the parent refresh fails, without accepting obsolete child responses.
    trajectoryPagesRef.current = retained;
    setTrajectoryPages(retained);
  }, []);

  const showLiveActivity = useCallback(() => {
    followingLive.current = true;
    setIsFollowingLive(true);
    manualExpansion.current = false;
    expansionRevision.current += 1;
    if (workingTrajectory.current) applyExpandedEventKey(workingTrajectory.current);
    liveRefresh.current = true;
    setPendingLiveActivity(false);
    setEventsAttempt((attempt) => attempt + 1);
  }, [applyExpandedEventKey]);

  const setFollowingLive = useCallback((following: boolean) => {
    followingLive.current = following;
    setIsFollowingLive(following);
    if (following) setPendingLiveActivity(false);
  }, []);

  const clearInputRefreshTimers = useCallback(() => {
    for (const timer of inputRefreshTimers.current) window.clearTimeout(timer);
    inputRefreshTimers.current = [];
  }, []);

  const refreshSessionAfterInput = useCallback((sessionKey: string) => {
    if (selectedSessionKeyRef.current !== sessionKey) return;
    clearInputRefreshTimers();
    const refresh = () => {
      if (selectedSessionKeyRef.current !== sessionKey) return;
      // Use the normal live path so loaded history, reading position and open
      // turns survive. The provider can acknowledge before persisting a message;
      // bounded follow-up reads also cover delayed writes and missed signals.
      liveRefresh.current = true;
      setPendingLiveActivity(!followingLive.current);
      if (eventRefreshInFlight.current) liveUpdateQueued.current = true;
      else setEventsAttempt((attempt) => attempt + 1);
    };
    refresh();
    inputRefreshTimers.current = INPUT_REFRESH_DELAYS_MS.map((delay) => window.setTimeout(refresh, delay));
  }, [clearInputRefreshTimers]);

  useEffect(() => clearInputRefreshTimers, [selectedSessionKey, clearInputRefreshTimers]);

  useEffect(() => {
    const following = !selectedSessionKey || !readReadingPosition(selectedSessionKey);
    followingLive.current = following;
    setIsFollowingLive(following);
    workingTrajectory.current = null;
    liveUpdateQueued.current = false;
    pendingLiveReset.current = false;
    uncommittedLiveReset.current = false;
    setPendingLiveActivity(false);
    setFollowError(null);
  }, [selectedSessionKey]);


  useEffect(() => {
    let disposed = false;
    let stop: (() => void) | undefined;
    void listenForSessionNotifications((notification) => {
      if (disposed) return;
      const update = (session: SessionSummary) => session.session_key === notification.session_key ? { ...session, ...notification } : session;
      setSessions((current) => current.map(update));
      setSelectedSessionMetadata((current) => current ? update(current) : current);
      const children = new Map(sessionChildrenRef.current);
      for (const [key, page] of children) children.set(key, { ...page, sessions: page.sessions.map(update) });
      sessionChildrenRef.current = children; setSessionChildren(children);
    }).then((unlisten) => {
      if (disposed) unlisten();
      else { stop = unlisten; notificationsReady.current = true; }
    }).catch(() => {});
    return () => { disposed = true; notificationsReady.current = false; stop?.(); };
  }, []);

  useEffect(() => {
    let disposed = false;
    let stop: (() => void) | undefined;
    const handle = (update: import("./types").SessionUpdate) => {
      if (disposed || !displayCache.current.accepts(update) || displayCache.current.stale(update)) return;
      const previousFinal = update.level === "final" ? displayCache.current.get(update.session_key, "final") : null;
      const page = displayCache.current.apply(update);
      if (update.session_key === selectedSessionKeyRef.current) {
        if (update.level === "details") setExpandedDetailError(update.state.error ?? null);
        else if (update.state.error) setEventsError(update.state.error);
      }
      if (!page) semanticLive.current.delete(update.session_key);
      if (update.level === "details") {
        for (const item of update.items) if (item.detail && item.event_key) writeCachedDetail(detailCache.current, `${update.session_key}:${item.event_key}`, item.detail);
        for (const key of update.removed_items) if (key.startsWith("detail:")) detailCache.current.delete(`${update.session_key}:${key.slice(7)}`);
        if (update.session_key === selectedSessionKeyRef.current && (update.items.some((item) => item.detail) || update.removed_items.length)) setDetailRevision((revision) => revision + 1);
        return;
      }
      {
        const questions = update.state.outstanding_questions ?? [];
        const question_attention = { required_count: 0, available_count: 0 };
        for (const question of questions) {
          if (question.requires_input) question_attention.required_count += question.unanswered_count;
          else question_attention.available_count += question.unanswered_count;
        }
        const newFinals = update.snapshot || update.level !== "final" ? 0 : update.items.filter((item) => item.kind === "assistant_message" && item.level === "final"
          && !previousFinal?.events.some((event) => event.event_key === item.item_id)).length;
        const updateSummary = (session: SessionSummary) => session.session_key === update.session_key
          ? { ...session, question_attention, is_running: update.state.is_running ?? session.is_running, has_unread: newFinals > 0 || session.has_unread, unread_final_count: Math.max(session.unread_final_count ?? 0, newFinals) } : session;
        setSessions((current) => current.map(updateSummary));
        setSelectedSessionMetadata((current) => current ? updateSummary(current) : current);
        const children = new Map(sessionChildrenRef.current);
        for (const [key, child] of children) children.set(key, { ...child, sessions: child.sessions.map(updateSummary) });
        sessionChildrenRef.current = children; setSessionChildren(children);
      }
      if (update.level === "final" || update.session_key !== selectedSessionKeyRef.current) return;
      pushedPage.current = page;
      pendingLiveReset.current ||= update.snapshot;
      liveRefresh.current = true;
      setPendingLiveActivity(!followingLive.current);
      if (eventRefreshInFlight.current) liveUpdateQueued.current = true;
      else setEventsAttempt((attempt) => attempt + 1);
    };
    onSessionUpdate.current = handle;
    void listenForSessionUpdates(handle).then((unlisten) => { if (disposed) unlisten(); else stop = unlisten; })
      .catch((error: unknown) => setEventsError(errorMessage(error)));
    return () => { disposed = true; stop?.(); };
  }, []);

  const detailSubscriptionKey = JSON.stringify([selectedSessionKey, activeDetailKeys.current]);
  useEffect(() => {
    const sessionKey = selectedSessionKeyRef.current;
    if (!sessionKey || !semanticLive.current.has(sessionKey) || displayCache.current.get(sessionKey, "all")) return;
    const keys = [...activeDetailKeys.current];
    if (!keys.length) {
      const release = displayCache.current.release(sessionKey, "details");
      if (release) void loadSessionUpdates(release).catch(() => {});
      return;
    }
    const refresh = () => {
      void loadSessionUpdates(displayCache.current.request(sessionKey, "details", keys)).then((update) => {
        displayCache.current.apply(update);
        for (const item of update.items) if (item.detail && item.event_key) writeCachedDetail(detailCache.current, `${sessionKey}:${item.event_key}`, item.detail);
        if (selectedSessionKeyRef.current === sessionKey && update.items.some((item) => item.detail)) setDetailRevision((revision) => revision + 1);
      }).catch(() => {});
    };
    refresh();
    const timer = window.setInterval(refresh, 30_000);
    return () => window.clearInterval(timer);
  }, [detailSubscriptionKey]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      if (semanticSupported.current !== true || !selectedSessionKeyRef.current || eventRefreshInFlight.current) return;
      for (const request of displayCache.current.backgroundRequests()) {
        void loadSessionUpdates(request).then((update) => onSessionUpdate.current(update)).catch(() => {});
      }
      liveRefresh.current = true;
      setEventsAttempt((attempt) => attempt + 1);
    }, 30_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listenForRelayChanges((change) => {
      if (disposed) return;
      if (change.session_key === null) setSessionsAttempt((attempt) => attempt + 1);
      if (!selectedSessionKeyRef.current) return;
      if (change.session_key === null && !change.reset) return;
      if (change.session_key !== null && change.session_key !== selectedSessionKeyRef.current) return;
      if (change.session_key !== null && !change.reset && semanticLive.current.has(change.session_key)) return;
      if (change.reset) {
        displayCache.current.invalidate(change.session_key ?? undefined);
        semanticLive.current.clear();
        pushedPage.current = null;
      }
      pendingLiveReset.current ||= change.reset;
      // Follow controls scrolling, not whether already displayed items update.
      // Keep tool/progress cards live while the reader is higher in the page.
      liveRefresh.current = true;
      setPendingLiveActivity(!followingLive.current);
      if (eventRefreshInFlight.current) liveUpdateQueued.current = true;
      else setEventsAttempt((attempt) => attempt + 1);
    }).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    }).catch((error: unknown) => setEventsError(errorMessage(error)));
    return () => { disposed = true; unlisten?.(); };
  }, [showLiveActivity]);

  useEffect(() => {
    if (eventsOwnerKey !== selectedSessionKey) return;
    const active = [...events].reverse().find((event) => event.trajectory?.status === "working")?.event_key ?? null;
    const previous = workingTrajectory.current;
    if (active !== previous) {
      workingTrajectory.current = active;
      if (followingLive.current) {
        // Only the turn that was working can finish automatically. A finished
        // turn reopened by the reader must survive newer activity unchanged.
        const expanded = expandedEventKeyRef.current;
        const finished = previous !== null && expanded === previous
          && events.find((event) => event.event_key === previous)?.trajectory?.status === "complete";
        if (finished) {
          manualExpansion.current = false;
          applyExpandedEventKey(active);
        } else if (active && !expanded) {
          manualExpansion.current = false;
          applyExpandedEventKey(active);
        }
      }
    }
  }, [applyExpandedEventKey, events, eventsOwnerKey, selectedSessionKey]);

  const requestTrajectoryEventPage = useCallback(
    (
      sessionKey: string,
      trajectoryKey: string,
      retry: boolean,
    ) => {
      const current = trajectoryPagesRef.current.get(sessionKey)?.get(trajectoryKey);
      if (current?.is_loading) {
        return;
      }
      if (current?.has_loaded && !retry) return;

      const generation = trajectoryPageGeneration.current;
      const requestKey = trajectoryRequestKey(sessionKey, trajectoryKey);
      const requestId = (trajectoryPageRequests.current.get(requestKey) ?? 0) + 1;
      trajectoryPageRequests.current.set(requestKey, requestId);
      updateTrajectoryPage(sessionKey, trajectoryKey, (existing) => {
        const page = existing ?? emptyTrajectoryEventPageState();
        return {
          ...page,
          is_loading: true,
          error: null,
        };
      });

      const working = eventsRef.current.find((event) => event.event_key === trajectoryKey)?.trajectory?.status === "working";
      const request = {
        session_key: sessionKey,
        trajectory_key: trajectoryKey,
        direction: working ? "backward" as const : "forward" as const,
        limit: TRAJECTORY_TRANSPORT_PAGE_SIZE,
      };
      const response = loadCompleteTrajectory(request, loadWorkPage,
        () => trajectoryPageGeneration.current === generation && trajectoryPageRequests.current.get(requestKey) === requestId);
      void response
        .then((response) => {
          if (
            trajectoryPageGeneration.current !== generation
            || trajectoryPageRequests.current.get(requestKey) !== requestId
          ) {
            return;
          }
          updateTrajectoryPage(sessionKey, trajectoryKey, (existing) => {
            const page = existing ?? emptyTrajectoryEventPageState();
            return {
              ...page,
              events: response.events,
              total_events: response.total_events,
              has_loaded: true,
              is_loading: false,
              error: null,
            };
          });
        })
        .catch((error: unknown) => {
          if (
            trajectoryPageGeneration.current !== generation
            || trajectoryPageRequests.current.get(requestKey) !== requestId
          ) {
            return;
          }
          updateTrajectoryPage(sessionKey, trajectoryKey, (existing) => {
            const page = existing ?? emptyTrajectoryEventPageState();
            return {
              ...page,
              is_loading: false,
              error: errorMessage(error),
            };
          });
        })
        .finally(() => {
          if (
            trajectoryPageGeneration.current === generation
            && trajectoryPageRequests.current.get(requestKey) === requestId
          ) {
            trajectoryPageRequests.current.delete(requestKey);
          }
        });
    },
    [loadWorkPage, updateTrajectoryPage],
  );

  const applyEventSelection = useCallback((eventKey: string | null, openInspector: boolean) => {
    if (selectedEventKeyRef.current !== eventKey) {
      selectedEventKeyRef.current = eventKey;
      detailRequest.current += 1;
      detailOwnerKeyRef.current = null;
      setSelectedEventKey(eventKey);
      setDetailOwnerKey(null);
      setDetail(null);
      setDetailLoading(false);
      setDetailError(null);
    }
    if (!eventKey) {
      inspectorTriggerRef.current = null;
      setInspectorOpen(false);
    } else if (openInspector) {
      setInspectorOpen(true);
    }
  }, []);

  const applySessionSelection = useCallback((sessionKey: string | null, metadata?: SessionSummary) => {
    if (selectedSessionKeyRef.current === sessionKey) {
      return;
    }
    const previousKey = selectedSessionKeyRef.current;
    if (previousKey) {
      sessionDisclosures.current.delete(previousKey);
      sessionDisclosures.current.set(previousKey, { expanded_event_key: expandedEventKeyRef.current, selected_event_key: selectedEventKeyRef.current, inspector_open: inspectorOpen, manual_expansion: manualExpansion.current });
      while (sessionDisclosures.current.size > 8) sessionDisclosures.current.delete(sessionDisclosures.current.keys().next().value!);
      for (const level of ["all", "steps", "details"] as const) {
        const release = displayCache.current.release(previousKey, level);
        if (release) void loadSessionUpdates(release).catch(() => {});
      }
      if (semanticLive.current.has(previousKey)) {
        void loadSessionUpdates(displayCache.current.request(previousKey, "final")).then((update) => onSessionUpdate.current(update)).catch(() => {});
      }
    }
    if (sessionKey) {
      const release = displayCache.current.release(sessionKey, "final");
      if (release) void loadSessionUpdates(release).catch(() => {});
    }
    displayCache.current.select(sessionKey);
    selectedSessionKeyRef.current = sessionKey;
    inspectorTriggerRef.current = null;
    eventsRequest.current += 1;
    eventsOwnerKeyRef.current = null;
    detailGeneration.current += 1;
    detailLoads.current.clear();
    expandedDetailRequest.current += 1;
    clearTrajectoryPages();
    setVisibleActivityGroups(new Set());
    setSelectedSessionKey(sessionKey);
    setSelectedSessionMetadata(metadata?.session_key === sessionKey ? metadata : null);
    setEventsOwnerKey(null);
    setInitialPageSessionKey(null);
    pushedPage.current = null;
    const cachedPage = sessionKey ? displayCache.current.get(sessionKey, "steps") : null;
    liveRefresh.current = !!cachedPage;
    if (cachedPage && sessionKey) {
      eventsOwnerKeyRef.current = sessionKey;
      setEventsOwnerKey(sessionKey);
      setEvents(cachedPage.events);
    } else setEvents([]);
    setEventsLoading(sessionKey !== null && !cachedPage);
    setOlderCursor(cachedPage?.previous_cursor ?? null);
    setNewerCursor(cachedPage?.next_cursor ?? null);
    setOlderLoading(false);
    setNewerLoading(false);
    setTotalEvents(cachedPage?.total_events ?? null);
    setHistoryStatus(cachedPage?.history_status ?? null);
    setOutstandingQuestions(cachedPage?.outstanding_questions ?? []);
    setPageQuestionAttention(undefined);
    setQuestionNavigation(null);
    setEventsError(null);
    manualExpansion.current = false;
    expansionRevision.current += 1;
    applyExpandedEventKey(null);
    expandedDetailOwnerKeyRef.current = null;
    setExpandedDetailOwnerKey(null);
    setExpandedDetail(null);
    setExpandedDetailLoading(false);
    setExpandedDetailError(null);
    applyEventSelection(null, false);
    const disclosure = sessionKey && cachedPage ? sessionDisclosures.current.get(sessionKey) : undefined;
    if (disclosure) {
      manualExpansion.current = disclosure.manual_expansion;
      applyExpandedEventKey(disclosure.expanded_event_key);
      applyEventSelection(disclosure.selected_event_key, disclosure.inspector_open);
    }
  }, [applyEventSelection, applyExpandedEventKey, clearTrajectoryPages, inspectorOpen]);

  const closeInspector = useCallback(() => {
    const trigger = inspectorTriggerRef.current;
    const fallbackEventKey = selectedEventKeyRef.current;
    detailRequest.current += 1;
    detailOwnerKeyRef.current = null;
    setInspectorOpen(false);
    setDetailOwnerKey(null);
    setDetail(null);
    setDetailLoading(false);
    setDetailError(null);
    window.requestAnimationFrame(() => {
      if (trigger?.isConnected) {
        trigger.focus();
      } else if (fallbackEventKey) {
        document.getElementById(eventButtonId(fallbackEventKey))?.focus();
      }
    });
  }, []);

  const updateSessionChildren = useCallback(
    (
      parentSessionKey: string,
      update: (current: SessionChildrenState | undefined) => SessionChildrenState,
    ) => {
      const next = new Map(sessionChildrenRef.current);
      next.set(parentSessionKey, update(next.get(parentSessionKey)));
      sessionChildrenRef.current = next;
      setSessionChildren(next);
    },
    [],
  );

  const clearSessionChildren = useCallback(() => {
    sessionChildrenGeneration.current += 1;
    sessionChildRequests.current.clear();
    const next = new Map<string, SessionChildrenState>();
    sessionChildrenRef.current = next;
    setSessionChildren(next);
  }, []);

  const requestSessionChildPage = useCallback(
    (parentSessionKey: string, cursor: string | null, retry: boolean) => {
      const current = sessionChildrenRef.current.get(parentSessionKey);
      if (cursor !== null) {
        if (
          !current
          || current.next_cursor !== cursor
          || current.is_loading
          || current.is_loading_more
        ) {
          return;
        }
      } else if (current && (current.is_loading || current.is_loading_more || !retry)) {
        return;
      }

      const generation = sessionChildrenGeneration.current;
      const requestId = (sessionChildRequests.current.get(parentSessionKey) ?? 0) + 1;
      sessionChildRequests.current.set(parentSessionKey, requestId);
      updateSessionChildren(parentSessionKey, (existing) => ({
        sessions: existing?.sessions ?? [],
        next_cursor: existing?.next_cursor ?? null,
        is_loading: cursor === null,
        is_loading_more: cursor !== null,
        error: null,
      }));

      void listSessionChildren({
        parent_session_key: parentSessionKey,
        cursor: cursor ?? undefined,
        limit: SESSION_PAGE_SIZE,
      })
        .then((response) => {
          if (
            sessionChildrenGeneration.current !== generation
            || sessionChildRequests.current.get(parentSessionKey) !== requestId
          ) {
            return;
          }
          updateSessionChildren(parentSessionKey, (existing) => ({
            // An activity card can optimistically materialize one exact child
            // before this lazy page arrives. Merge both initial and later
            // pages so that navigation never makes that child disappear.
            sessions: mergeSessions(existing?.sessions ?? [], response.sessions),
            next_cursor: response.next_cursor,
            is_loading: false,
            is_loading_more: false,
            error: null,
          }));
        })
        .catch((error: unknown) => {
          if (
            sessionChildrenGeneration.current !== generation
            || sessionChildRequests.current.get(parentSessionKey) !== requestId
          ) {
            return;
          }
          updateSessionChildren(parentSessionKey, (existing) => ({
            sessions: existing?.sessions ?? [],
            next_cursor: existing?.next_cursor ?? null,
            is_loading: false,
            is_loading_more: false,
            error: errorMessage(error),
          }));
        })
        .finally(() => {
          if (
            sessionChildrenGeneration.current === generation
            && sessionChildRequests.current.get(parentSessionKey) === requestId
          ) {
            sessionChildRequests.current.delete(parentSessionKey);
          }
        });
    },
    [updateSessionChildren],
  );

  const loadSessionChildren = useCallback((parentSessionKey: string) => {
    requestSessionChildPage(parentSessionKey, null, false);
  }, [requestSessionChildPage]);

  const retrySessionChildren = useCallback((parentSessionKey: string) => {
    const current = sessionChildrenRef.current.get(parentSessionKey);
    requestSessionChildPage(parentSessionKey, current?.next_cursor ?? null, true);
  }, [requestSessionChildPage]);

  const loadMoreSessionChildren = useCallback((parentSessionKey: string) => {
    const cursor = sessionChildrenRef.current.get(parentSessionKey)?.next_cursor;
    if (cursor) {
      requestSessionChildPage(parentSessionKey, cursor, false);
    }
  }, [requestSessionChildPage]);

  const applyQuestionPage = useCallback((sessionKey: string, questions: OutstandingQuestion[] | undefined) => {
    setOutstandingQuestions(questions ?? []);
    if (questions === undefined) {
      setPageQuestionAttention(undefined);
      return;
    }
    const question_attention = { required_count: 0, available_count: 0 };
    for (const question of questions) {
      if (question.requires_input) question_attention.required_count += question.unanswered_count;
      else question_attention.available_count += question.unanswered_count;
    }
    setPageQuestionAttention(question_attention);
    const update = (session: SessionSummary) => session.session_key === sessionKey
      ? { ...session, question_attention }
      : session;
    // Event pages are fresher than the background catalog. Keep cached child
    // rows in sync too, so switching sessions cannot restore their old badge.
    setSessions((current) => current.map(update));
    setSelectedSessionMetadata((current) => current ? update(current) : current);
    const children = new Map(sessionChildrenRef.current);
    let childrenChanged = false;
    for (const [parent, state] of children) {
      if (state.sessions.some((session) => session.session_key === sessionKey)) {
        children.set(parent, { ...state, sessions: state.sessions.map(update) });
        childrenChanged = true;
      }
    }
    if (childrenChanged) {
      sessionChildrenRef.current = children;
      setSessionChildren(children);
    }
  }, []);

  // A catalog refresh may still contain the previous attention counts while
  // indexing catches up. The accepted page owns the selected session's badge.
  const displayedSessions = useMemo(() => {
    if (pageQuestionAttention === undefined) return { sessions, children: sessionChildren };
    const update = (session: SessionSummary) => pageQuestionAttention !== undefined
      && session.session_key === selectedSessionKey
      ? { ...session, question_attention: pageQuestionAttention }
      : session;
    return {
      sessions: sessions.map(update),
      children: new Map([...sessionChildren].map(([parent, state]) => [
        parent, { ...state, sessions: state.sessions.map(update) },
      ])),
    };
  }, [pageQuestionAttention, selectedSessionKey, sessionChildren, sessions]);

  const selectedSession = useMemo(
    () => findKnownSession(displayedSessions.sessions, displayedSessions.children, selectedSessionKey)
      ?? (selectedSessionMetadata?.session_key === selectedSessionKey ? selectedSessionMetadata : null),
    [selectedSessionKey, selectedSessionMetadata, displayedSessions],
  );
  useSessionView(selectedSessionKey, selectedSession, sessions, sessionChildren);

  const openRelatedSession = useCallback((sourceSessionKey: string, target: SessionSummary) => {
    // The target came from an activity event in this exact source timeline.
    // Ignore a stale card after the user has already selected another session.
    if (selectedSessionKeyRef.current !== sourceSessionKey
      || selectedSession?.session_key !== sourceSessionKey) {
      return;
    }
    const isDirectChild = target.session_key !== sourceSessionKey
      && target.provider === selectedSession.provider
      && target.is_subagent
      && target.parent_session_id === selectedSession.session_id;
    if (isDirectChild) {
      updateSessionChildren(sourceSessionKey, (existing) => ({
        sessions: mergeSessions(existing?.sessions ?? [], [target]),
        next_cursor: existing?.next_cursor ?? null,
        is_loading: existing?.is_loading ?? false,
        is_loading_more: existing?.is_loading_more ?? false,
        error: existing?.error ?? null,
      }));
      // Fetch the normal sidebar page for verified direct children only.
      // Parent, sibling, and deeper sender links must not rewrite this tree.
      requestSessionChildPage(sourceSessionKey, null, true);
    }
    applySessionSelection(target.session_key, target);
    setMobileSidebarOpen(false);
  }, [applySessionSelection, requestSessionChildPage, selectedSession, updateSessionChildren]);

  useEffect(() => {
    if (!sessionIndexListenerReady) {
      return;
    }
    const queryChanged = previousSessionQueryKey.current !== sessionQueryKey;
    const filterKey = `${providerKey}\u0000${debouncedSearch}`;
    const filtersChanged = previousSessionFilterKey.current !== filterKey;
    if (!queryChanged && sessionListInFlight.current === sessionsRequest.current) {
      // Index and Relay notifications can outpace a catalog read. Keep its
      // result useful and coalesce those notifications into one trailing
      // refresh instead of starting concurrent reads that will be discarded.
      sessionListRefreshQueued.current = true;
      return;
    }
    const requestId = ++sessionsRequest.current;
    sessionListInFlight.current = null;
    sessionListRefreshQueued.current = false;
    previousSessionQueryKey.current = sessionQueryKey;
    previousSessionFilterKey.current = filterKey;
    setSessionsLoadingMore(false);
    if (enabledProviders.size === 0) {
      clearSessionChildren();
      setSessions([]);
      setSessionsCursor(null);
      setSessionsLoading(false);
      setSessionsError(null);
      setSourceErrors([]);
      setPendingProviders([]);
      applySessionSelection(null);
      return;
    }

    setSessionsLoading(true);
    setSessionsError(null);
    sessionListInFlight.current = requestId;
    void listSessions({
      query: {
        order: sessionOrder,
        providers: PROVIDERS.filter((provider) => enabledProviders.has(provider)),
        search: debouncedSearch || undefined,
      },
      limit: SESSION_PAGE_SIZE,
    })
      .then((response) => {
        if (sessionsRequest.current !== requestId) {
          return;
        }
        setSessions(response.sessions);
        setSessionsCursor(response.next_cursor);
        setSourceErrors(response.source_errors);
        setPendingProviders(response.pending_providers);
        if (filtersChanged) {
          // Root responses deliberately omit lazy child rows. Replace the
          // tree only once the changed query is accepted, retaining an
          // explicit root selection only when it still belongs to the new
          // first page. Background index refreshes keep the cached tree and
          // can therefore leave a selected child visible.
          clearSessionChildren();
          const currentSelection = selectedSessionKeyRef.current;
          const selectedRootIsVisible = currentSelection !== null
            && response.sessions.some((session) => session.session_key === currentSelection);
          applySessionSelection(selectedRootIsVisible ? currentSelection : null);
        } else {
          applySessionSelection(preserveSessionSelection(selectedSessionKeyRef.current));
        }
      })
      .catch((error: unknown) => {
        if (sessionsRequest.current === requestId) {
          if (filtersChanged) {
            clearSessionChildren();
            setSessions([]);
            setSessionsCursor(null);
            setSourceErrors([]);
            setPendingProviders([]);
            applySessionSelection(null);
          }
          setSessionsError(errorMessage(error));
        }
      })
      .finally(() => {
        if (sessionsRequest.current === requestId) {
          setSessionsLoading(false);
        }
        finishSessionListRequest(requestId);
      });
  }, [
    applySessionSelection,
    clearSessionChildren,
    debouncedSearch,
    enabledProviders,
    finishSessionListRequest,
    providerKey,
    sessionQueryKey,
    sessionOrder,
    sessionIndexListenerReady,
    sessionsAttempt,
  ]);

  useEffect(() => {
    const requestId = ++eventsRequest.current;
    // An acknowledgement belongs to one exact newest-page request. A retry
    // or selected-session change invalidates any response that has not yet
    // reached a committed React render.
    setAcceptedInitialEventPage(null);
    const ownsSession = eventsOwnerKeyRef.current === selectedSessionKey;
    // A failed or superseded replacement keeps its reset semantics until commit.
    const reset = pendingLiveReset.current || uncommittedLiveReset.current;
    const isLiveRefresh = (liveRefresh.current || reset) && ownsSession;
    liveRefresh.current = false;
    pendingLiveReset.current = false;
    uncommittedLiveReset.current = reset;
    if (!isLiveRefresh) clearTrajectoryPages();
    else invalidateTrajectoryPages(false);
    if (!ownsSession) {
      eventsOwnerKeyRef.current = selectedSessionKey;
      setEventsOwnerKey(selectedSessionKey);
      setEvents([]);
      setInitialPageSessionKey(null);
      applyEventSelection(null, false);
    }
    if (!isLiveRefresh) {
      setOlderCursor(null);
      setNewerCursor(null);
    }
    setOlderLoading(false);
    setNewerLoading(false);
    if (!isLiveRefresh) {
      setTotalEvents(null);
      setHistoryStatus(null);
      setOutstandingQuestions([]);
      setPageQuestionAttention(undefined);
      setQuestionNavigation(null);
    }
    setEventsError(null);

    if (!selectedSessionKey) {
      eventRefreshInFlight.current = false;
      setEventsLoading(false);
      return;
    }

    setEventsLoading(true);
    eventRefreshInFlight.current = true;
    const refreshExpansionRevision = expansionRevision.current;
    const pushed = pushedPage.current;
    pushedPage.current = null;
    const page = isLiveRefresh
      ? pushed ? Promise.resolve(pushed) : refreshEventWindow(selectedSessionKey, loadDisplayPage)
      : loadReadingWindow(selectedSessionKey, readReadingPosition(selectedSessionKey), loadDisplayPage,
        () => eventsRequest.current === requestId);
    void page
      .then(async (response) => {
        if (eventsRequest.current !== requestId) {
          return;
        }
        if (!isLiveRefresh) {
          const position = readReadingPosition(selectedSessionKey);
          const last = response.events[response.events.length - 1];
          const following = !position || (position.at_end && !!last && position.last_event === readingEventKey(last));
          followingLive.current = following;
          setIsFollowingLive(following);
          setPendingLiveActivity(!!position && !!last && position.last_event !== readingEventKey(last));
        }
        const active = [...response.events].reverse().find((event) => event.trajectory?.status === "working");
        let replacement: { trajectory_key: string; page: TrajectoryEventPageResponse } | null = null;
        let finishedExpandedTurn = false;
        while (isLiveRefresh && reset) {
          const revision = expansionRevision.current;
          const expanded = eventsRef.current.find((event) => event.event_key === expandedEventKeyRef.current);
          // Disclosure state can stay open for a surviving trajectory slot.
          // Its old child rows and detail ownership are never reused on reset.
          const retained = expanded?.type === "trajectory"
            ? response.events.find((event) => (event.slot_key ?? event.event_key) === (expanded.slot_key ?? expanded.event_key)
              && event.type === "trajectory" && event.provider === expanded.provider)
            : undefined;
          finishedExpandedTurn = followingLive.current && expanded?.event_key === workingTrajectory.current
            && expanded?.trajectory?.status === "working" && retained?.trajectory?.status === "complete";
          const autoOpen = followingLive.current && (!manualExpansion.current
            || (revision === refreshExpansionRevision && active?.event_key !== workingTrajectory.current));
          const target = (finishedExpandedTurn ? undefined : retained) ?? (autoOpen ? active : undefined);
          replacement = null;
          if (!target) break;
          const selectionIsCurrent = () => expansionRevision.current === revision
            && (target === retained || followingLive.current);
          // Publish replacement summaries and complete group contents together;
          // preserve the last good view until every legacy transport page arrives.
          let refreshed: TrajectoryEventPageResponse;
          try {
            refreshed = await loadCompleteTrajectory({
              session_key: selectedSessionKey,
              trajectory_key: target.event_key,
              direction: target.trajectory?.status === "working"
                ? "backward" : "forward",
              limit: TRAJECTORY_TRANSPORT_PAGE_SIZE,
            }, loadWorkPage,
            () => eventsRequest.current === requestId && selectionIsCurrent());
          } catch (error: unknown) {
            if (eventsRequest.current !== requestId) return;
            if (!selectionIsCurrent()) continue;
            throw error;
          }
          if (eventsRequest.current !== requestId) return;
          // A disclosure click during loading wins over the staged choice.
          if (!selectionIsCurrent()) continue;
          replacement = { trajectory_key: target.event_key, page: refreshed };
          break;
        }
        if (reset) {
          // Publish the replacement as one React update. Old source positions
          // cannot continue to own expanded detail or Inspector selection.
          uncommittedLiveReset.current = false;
          clearTrajectoryPages();
          applyEventSelection(null, false);
          workingTrajectory.current = active?.event_key ?? null;
          if (finishedExpandedTurn) manualExpansion.current = false;
          applyExpandedEventKey(replacement?.trajectory_key ?? null);
          if (replacement) {
            const ready = replacement;
            updateTrajectoryPage(selectedSessionKey, ready.trajectory_key, () => ({
              ...emptyTrajectoryEventPageState(),
              events: ready.page.events,
              total_events: ready.page.total_events,
              has_loaded: true,
            }));
          }
        }
        const changed = displayCache.current.commit(selectedSessionKey, response);
        if (isLiveRefresh && !reset && semanticLive.current.has(selectedSessionKey)) {
          const previousEvents = new Map(events.map((event) => [event.event_key, event]));
          for (const key of changed ?? []) detailCache.current.delete(`${selectedSessionKey}:${key}`);
          for (const event of response.events) {
            if (previousEvents.get(event.event_key) !== event) detailCache.current.delete(`${selectedSessionKey}:${event.event_key}`);
          }
          detailGeneration.current += 1;
          detailLoads.current.clear();
          setDetailRevision((revision) => revision + 1);
        } else invalidateEventDetails(isLiveRefresh && !reset);
        if (isLiveRefresh && !reset) {
          // Invalidate in-flight child reads, but retain their displayed rows
          // until a fresh bounded child page arrives (no loading flicker).
          invalidateTrajectoryPages(true, changed ?? undefined);
        }
        const sourceError = (response as import("./types").EventPageResponse & { error?: string }).error;
        if (sourceError) setEventsError(sourceError);
        setEvents(response.events);
        setOlderCursor(response.previous_cursor);
        setNewerCursor(response.next_cursor);
        setTotalEvents(response.total_events);
        setHistoryStatus(response.history_status);
        setFollowError(response.follow_error ?? null);
        applyQuestionPage(selectedSessionKey, response.outstanding_questions);
        setInitialPageSessionKey(selectedSessionKey);
        if (pendingQuestionSession.current === selectedSessionKey) {
          pendingQuestionSession.current = null;
          const question = response.outstanding_questions?.[0];
          if (question) {
            manualExpansion.current = true;
            expansionRevision.current += 1;
            applyExpandedEventKey(question.event_key);
            setQuestionNavigation((current) => ({ event_key: question.event_key, revision: (current?.revision ?? 0) + 1 }));
          }
        }
        applyEventSelection(
          preserveEventSelection(selectedEventKeyRef.current, response.events),
          false,
        );
        if (response.attention_revision) {
          setAcceptedInitialEventPage({
            sessionKey: selectedSessionKey,
            requestId,
            attentionRevision: response.attention_revision,
          });
        }
      })
      .catch((error: unknown) => {
        if (eventsRequest.current === requestId) {
          setEventsError(errorMessage(error));
        }
      })
      .finally(() => {
        if (eventsRequest.current === requestId) {
          eventRefreshInFlight.current = false;
          setEventsLoading(false);
          if (liveUpdateQueued.current) {
            liveUpdateQueued.current = false;
            liveRefresh.current = true;
            setEventsAttempt((attempt) => attempt + 1);
          }
        }
      });
  }, [
    applyEventSelection,
    applyExpandedEventKey,
    applyQuestionPage,
    clearTrajectoryPages,
    eventsAttempt,
    invalidateEventDetails,
    invalidateTrajectoryPages,
    loadDisplayPage,
    loadWorkPage,
    selectedSessionKey,
    updateTrajectoryPage,
  ]);

  useEffect(() => {
    if (!acceptedInitialEventPage) {
      return;
    }

    const { attentionRevision, requestId, sessionKey } = acceptedInitialEventPage;
    const matchesCommittedCurrentPage = (
      selectedSessionKey === sessionKey
      && selectedSessionKeyRef.current === sessionKey
      && eventsOwnerKey === sessionKey
      && eventsOwnerKeyRef.current === sessionKey
      && initialPageSessionKey === sessionKey
      && eventsRequest.current === requestId
    );
    if (!matchesCommittedCurrentPage) {
      // A new selection or refresh won before this page was rendered. Do not
      // advance the seen cursor for a timeline the user never actually saw.
      setAcceptedInitialEventPage((current) => (
        current?.requestId === requestId ? null : current
      ));
      return;
    }

    if (!isFollowingLive || !followingLive.current) return;

    if (acknowledgedInitialPageRequest.current === requestId) {
      return;
    }
    // Consume the request before the asynchronous IPC call. This keeps a
    // Strict Mode effect replay or an unrelated render from issuing a second
    // acknowledgement for the same accepted page.
    acknowledgedInitialPageRequest.current = requestId;
    setAcceptedInitialEventPage((current) => (
      current?.requestId === requestId ? null : current
    ));
    acknowledgeAcceptedAttention(sessionKey, attentionRevision);
  }, [
    acceptedInitialEventPage,
    isFollowingLive,
    acknowledgeAcceptedAttention,
    eventsOwnerKey,
    initialPageSessionKey,
    selectedSessionKey,
  ]);

  useEffect(() => {
    const requestId = ++detailRequest.current;
    setDetailError(null);

    if (!inspectorOpen || !selectedSessionKey || !selectedEventKey) {
      detailOwnerKeyRef.current = null;
      setDetailOwnerKey(null);
      setDetail(null);
      setDetailLoading(false);
      return;
    }

    const cacheKey = `${selectedSessionKey}:${selectedEventKey}`;
    if (detailOwnerKeyRef.current !== cacheKey) setDetail(null);
    detailOwnerKeyRef.current = cacheKey;
    setDetailOwnerKey(cacheKey);
    const cached = readCachedDetail(detailCache.current, cacheKey);
    if (cached) {
      setDetail(cached);
      setDetailLoading(false);
      return;
    }

    setDetailLoading(true);
    void requestDetail(selectedSessionKey, selectedEventKey)
      .then((response) => {
        if (detailRequest.current !== requestId) {
          return;
        }
        setDetail(response);
      })
      .catch((error: unknown) => {
        if (detailRequest.current === requestId) {
          setDetailError(errorMessage(error));
        }
      })
      .finally(() => {
        if (detailRequest.current === requestId) {
          setDetailLoading(false);
        }
      });
  }, [
    detailAttempt,
    detailRevision,
    inspectorOpen,
    requestDetail,
    selectedEventKey,
    selectedSessionKey,
  ]);

  useEffect(() => {
    const requestId = ++expandedDetailRequest.current;
    setExpandedDetailError(null);

    const expandedEvent = events.find((event) => event.event_key === expandedEventKey);
    if (!selectedSessionKey || !expandedEventKey || !expandedEventNeedsDetail(expandedEvent)) {
      expandedDetailOwnerKeyRef.current = null;
      setExpandedDetailOwnerKey(null);
      setExpandedDetail(null);
      setExpandedDetailLoading(false);
      return;
    }

    const cacheKey = `${selectedSessionKey}:${expandedEventKey}`;
    if (expandedDetailOwnerKeyRef.current !== cacheKey) setExpandedDetail(null);
    expandedDetailOwnerKeyRef.current = cacheKey;
    setExpandedDetailOwnerKey(cacheKey);
    const cached = readCachedDetail(detailCache.current, cacheKey);
    if (cached) {
      setExpandedDetail(cached);
      setExpandedDetailLoading(false);
      return;
    }

    setExpandedDetailLoading(true);
    void requestDetail(selectedSessionKey, expandedEventKey)
      .then((response) => {
        if (expandedDetailRequest.current === requestId) {
          setExpandedDetail(response);
        }
      })
      .catch((error: unknown) => {
        if (expandedDetailRequest.current === requestId) {
          setExpandedDetailError(errorMessage(error));
        }
      })
      .finally(() => {
        if (expandedDetailRequest.current === requestId) {
          setExpandedDetailLoading(false);
        }
      });
  }, [
    detailRevision,
    events,
    expandedDetailAttempt,
    expandedEventKey,
    requestDetail,
    selectedSessionKey,
  ]);

  useEffect(() => {
    if (!selectedSessionKey || !expandedEventKey) {
      return;
    }
    const event = events.find((candidate) => candidate.event_key === expandedEventKey);
    if (event?.type !== "trajectory") {
      return;
    }
    requestTrajectoryEventPage(
      selectedSessionKey,
      event.event_key,
      false,
    );
  }, [detailRevision, events, expandedEventKey, requestTrajectoryEventPage, selectedSessionKey]);

  useEffect(() => {
    const requestId = ++expandedTrajectoryDetailRequest.current;
    if (!selectedSessionKey || !expandedEventKey) return;
    const pages = trajectoryPages.get(selectedSessionKey);
    const children = pages?.get(expandedEventKey)?.events.flatMap((child) => child.type === "activity_group"
      ? pages.get(child.event_key)?.events ?? [] : [child]) ?? [];
    for (const child of children) {
      if (!activeDetailKeys.current.includes(child.event_key) || !expandedEventNeedsDetail(child)) continue;
      const cacheKey = `${selectedSessionKey}:${child.event_key}`;
      const cached = readCachedDetail(detailCache.current, cacheKey);
      setExpandedActivities((current) => {
        const next = new Map(current);
        next.set(child.event_key, { detail: cached ?? current.get(child.event_key)?.detail ?? null,
          error: null, is_loading: !cached });
        return next;
      });
      if (cached) continue;
      void requestDetail(selectedSessionKey, child.event_key).then((detail) => {
        if (expandedTrajectoryDetailRequest.current !== requestId) return;
        setExpandedActivities((current) => new Map(current).set(child.event_key,
          { detail, error: null, is_loading: false }));
      }).catch((error: unknown) => {
        if (expandedTrajectoryDetailRequest.current !== requestId) return;
        setExpandedActivities((current) => new Map(current).set(child.event_key,
          { detail: current.get(child.event_key)?.detail ?? null, error: errorMessage(error), is_loading: false }));
      });
    }
  }, [detailRevision, expandedTrajectoryDetailAttempt, expandedActivityKeys,
    expandedEventKey, requestDetail, selectedSessionKey, trajectoryPages, detailSubscriptionKey]);

  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== "Escape") {
        return;
      }
      if (mobileSidebarOpen) {
        setMobileSidebarOpen(false);
      } else if (inspectorOpen) {
        closeInspector();
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [closeInspector, inspectorOpen, mobileSidebarOpen]);

  const eventsAreOwned = selectedSessionKey !== null && eventsOwnerKey === selectedSessionKey;
  const visibleEvents = eventsAreOwned ? events : [];
  const selectedEvent = useMemo(() => {
    const timelineEvent = visibleEvents.find((event) => event.event_key === selectedEventKey) ?? null;
    if (timelineEvent || !selectedSessionKey || !selectedEventKey) {
      return timelineEvent;
    }
    const pages = trajectoryPages.get(selectedSessionKey);
    if (!pages) {
      return null;
    }
    for (const page of pages.values()) {
      const trajectoryEvent = page.events.find((event) => event.event_key === selectedEventKey);
      if (trajectoryEvent) {
        return trajectoryEvent;
      }
    }
    return null;
  }, [selectedEventKey, selectedSessionKey, trajectoryPages, visibleEvents]);
  const detailTargetKey = selectedSessionKey && selectedEventKey
    ? `${selectedSessionKey}:${selectedEventKey}`
    : null;
  const detailIsOwned = detailTargetKey !== null && detailOwnerKey === detailTargetKey;
  const expandedVisibleEvent = expandedEventKey === null
    ? null
    : visibleEvents.find((event) => event.event_key === expandedEventKey) ?? null;
  const expandedEventIsVisible = expandedVisibleEvent !== null;
  const expandedDetailTargetKey = selectedSessionKey
    && expandedEventNeedsDetail(expandedVisibleEvent)
    ? `${selectedSessionKey}:${expandedEventKey}`
    : null;
  const expandedDetailIsOwned = expandedDetailTargetKey !== null
    && expandedDetailOwnerKey === expandedDetailTargetKey;
  const expandedTrajectoryChild = useMemo(() => {
    if (
      !selectedSessionKey
      || !expandedTrajectoryEvent
      || !expandedActivityKeys.has(expandedTrajectoryEvent.event_key)
      || expandedEventKey !== expandedTrajectoryEvent.trajectory_key
    ) {
      return null;
    }
    return trajectoryPages
      .get(selectedSessionKey)
      ?.get(expandedTrajectoryEvent.trajectory_key)
      ?.events.find((event) => event.event_key === expandedTrajectoryEvent.event_key)
      ?? null;
  }, [expandedActivityKeys, expandedEventKey, expandedTrajectoryEvent, selectedSessionKey, trajectoryPages]);
  const currentActivity = expandedTrajectoryChild
    ? expandedActivities.get(expandedTrajectoryChild.event_key) : null;
  const expandedTrajectoryDetail = currentActivity?.detail ?? null;
  const expandedTrajectoryDetailError = currentActivity?.error ?? null;
  const expandedTrajectoryDetailLoading = expandedTrajectoryChild !== null
    && expandedEventNeedsDetail(expandedTrajectoryChild) && (currentActivity?.is_loading ?? true);

  const toggleProvider = useCallback((provider: ViewerProvider) => {
    sessionsRequest.current += 1;
    setSessionsLoading(true);
    setEnabledProviders((current) => {
      const next = new Set(current);
      if (next.has(provider)) {
        next.delete(provider);
      } else {
        next.add(provider);
      }
      return next;
    });
  }, []);

  const changeSessionOrder = useCallback((order: SessionOrder) => {
    if (order === sessionOrder) return;
    sessionsRequest.current += 1;
    sessionListInFlight.current = null;
    sessionListRefreshQueued.current = false;
    setSessionsLoading(true);
    setSessionsCursor(null);
    setSelectedSessionMetadata(selectedSession);
    setSessionOrder(order);
    saveSessionOrder(order);
  }, [sessionOrder, selectedSession]);

  const changeSearch = useCallback((value: string) => {
    setSearchValue(value);
  }, []);

  const selectSession = useCallback((sessionKey: string) => {
    pendingQuestionSession.current = null;
    applySessionSelection(sessionKey);
    setMobileSidebarOpen(false);
  }, [applySessionSelection]);

  const openQuestion = useCallback((eventKey: string) => {
    manualExpansion.current = true;
    expansionRevision.current += 1;
    applyExpandedEventKey(eventKey);
    setQuestionNavigation((current) => ({ event_key: eventKey, revision: (current?.revision ?? 0) + 1 }));
  }, [applyExpandedEventKey]);

  const selectQuestionSession = useCallback((sessionKey: string) => {
    if (sessionKey === selectedSessionKey && outstandingQuestions.length) {
      openQuestion(outstandingQuestions[0].event_key);
    } else {
      pendingQuestionSession.current = sessionKey;
      applySessionSelection(sessionKey);
    }
    setMobileSidebarOpen(false);
  }, [applySessionSelection, openQuestion, outstandingQuestions, selectedSessionKey]);

  const selectEvent = useCallback((eventKey: string) => {
    inspectorTriggerRef.current = document.getElementById(eventButtonId(eventKey));
    applyEventSelection(eventKey, true);
  }, [applyEventSelection]);

  const toggleEventExpanded = useCallback((eventKey: string) => {
    manualExpansion.current = true;
    expansionRevision.current += 1;
    applyExpandedEventKey(expandedEventKeyRef.current === eventKey ? null : eventKey);
  }, [applyExpandedEventKey]);

  const toggleTrajectoryEventExpanded = useCallback((trajectoryKey: string, eventKey: string) => {
    setExpandedActivityKeys((current) => {
      const next = new Set(current);
      if (next.has(eventKey)) next.delete(eventKey);
      else next.add(eventKey);
      return next;
    });
    setExpandedTrajectoryEvent({ trajectory_key: trajectoryKey, event_key: eventKey });
    setExpandedActivities((current) => {
      const next = new Map(current);
      next.delete(eventKey);
      return next;
    });
  }, []);

  const retryExpandedTrajectoryDetail = useCallback((trajectoryKey: string, eventKey: string) => {
    setExpandedTrajectoryEvent({ trajectory_key: trajectoryKey, event_key: eventKey });
    setExpandedActivityKeys((current) => new Set(current).add(eventKey));
    setExpandedTrajectoryDetailAttempt((attempt) => attempt + 1);
  }, []);

  const loadActivityGroup = useCallback((groupKey: string) => {
    const sessionKey = selectedSessionKeyRef.current;
    if (sessionKey) requestTrajectoryEventPage(sessionKey, groupKey, false);
  }, [requestTrajectoryEventPage]);

  const retryTrajectoryEvents = useCallback((trajectoryKey: string) => {
    if (!selectedSessionKey) {
      return;
    }
    requestTrajectoryEventPage(selectedSessionKey, trajectoryKey, true);
  }, [requestTrajectoryEventPage, selectedSessionKey]);

  const toggleInspector = useCallback(() => {
    if (inspectorOpen) {
      closeInspector();
      return;
    }
    if (!selectedEventKeyRef.current) {
      return;
    }
    if (document.activeElement instanceof HTMLElement) {
      inspectorTriggerRef.current = document.activeElement;
    }
    setInspectorOpen(true);
  }, [closeInspector, inspectorOpen]);

  const loadMoreSessions = useCallback(() => {
    if (!sessionsCursor || sessionsLoading || sessionsLoadingMore || sessionListInFlight.current !== null) {
      return;
    }
    const requestGeneration = sessionsRequest.current;
    sessionListInFlight.current = requestGeneration;
    setSessionsLoadingMore(true);
    setSessionsError(null);
    void listSessions({
      query: {
        order: sessionOrder,
        providers: PROVIDERS.filter((provider) => enabledProviders.has(provider)),
        search: debouncedSearch || undefined,
      },
      cursor: sessionsCursor,
      limit: SESSION_PAGE_SIZE,
    })
      .then((response) => {
        if (sessionsRequest.current !== requestGeneration) {
          return;
        }
        setSessions((current) => {
          const merged = mergeSessions(current, response.sessions);
          return sessionOrder === "project" ? merged.sort(compareProjects) : merged;
        });
        setSessionsCursor(response.next_cursor);
        setSourceErrors(response.source_errors);
        setPendingProviders(response.pending_providers);
      })
      .catch((error: unknown) => {
        if (sessionsRequest.current === requestGeneration) {
          setSessionsError(errorMessage(error));
        }
      })
      .finally(() => {
        if (sessionsRequest.current === requestGeneration) {
          setSessionsLoadingMore(false);
        }
        finishSessionListRequest(requestGeneration);
      });
  }, [debouncedSearch, enabledProviders, finishSessionListRequest, sessionOrder, sessionsCursor, sessionsLoading, sessionsLoadingMore]);

  const loadOlderEvents = useCallback(() => {
    if (
      !selectedSessionKey
      || eventsOwnerKeyRef.current !== selectedSessionKey
      || !olderCursor
      || olderLoading
      || eventRefreshInFlight.current
    ) {
      return;
    }
    const requestGeneration = eventsRequest.current;
    eventRefreshInFlight.current = true;
    setOlderLoading(true);
    setEventsError(null);
    void loadDisplayPage({
      session_key: selectedSessionKey,
      cursor: olderCursor,
      window_mode: "earlier",
      direction: "backward",
    })
      .then((response) => {
        if (eventsRequest.current !== requestGeneration) {
          return;
        }
        invalidateEventDetails(true);
        setEvents(response.events);
        setOlderCursor(response.previous_cursor);
        setNewerCursor(response.next_cursor);
        setTotalEvents(response.total_events);
        setHistoryStatus(response.history_status);
        applyQuestionPage(selectedSessionKey, response.outstanding_questions);
      })
      .catch((error: unknown) => {
        if (eventsRequest.current === requestGeneration) {
          setEventsError(errorMessage(error));
        }
      })
      .finally(() => {
        if (eventsRequest.current === requestGeneration) {
          eventRefreshInFlight.current = false;
          setOlderLoading(false);
          if (liveUpdateQueued.current) {
            liveUpdateQueued.current = false;
            liveRefresh.current = true;
            setEventsAttempt((attempt) => attempt + 1);
          }
        }
      });
  }, [applyQuestionPage, invalidateEventDetails, olderCursor, olderLoading, selectedSessionKey, loadDisplayPage]);

  const loadNewerEvents = useCallback(() => {
    if (
      !selectedSessionKey
      || eventsOwnerKeyRef.current !== selectedSessionKey
      || !newerCursor
      || newerLoading
    ) {
      return;
    }
    const requestGeneration = eventsRequest.current;
    setNewerLoading(true);
    setEventsError(null);
    void loadEventPage({
      session_key: selectedSessionKey,
      cursor: newerCursor,
      direction: "forward",
      limit: EVENT_PAGE_SIZE,
    })
      .then((response) => {
        if (eventsRequest.current !== requestGeneration) {
          return;
        }
        invalidateEventDetails(true);
        setEvents((current) => mergeEvents(current, response.events, "after"));
        setNewerCursor(response.next_cursor);
        setTotalEvents(response.total_events);
        applyQuestionPage(selectedSessionKey, response.outstanding_questions);
      })
      .catch((error: unknown) => {
        if (eventsRequest.current === requestGeneration) {
          setEventsError(errorMessage(error));
        }
      })
      .finally(() => {
        if (eventsRequest.current === requestGeneration) {
          setNewerLoading(false);
        }
      });
  }, [applyQuestionPage, invalidateEventDetails, newerCursor, newerLoading, selectedSessionKey]);

  const retrySessions = useCallback(() => {
    // Preserve the old immediate SQLite reread so a previously committed
    // catalog remains visible, while also waking the actual provider indexer.
    setSessionsAttempt((attempt) => attempt + 1);
    void retrySessionIndex();
  }, [retrySessionIndex]);

  return {
    sessionOrder,
    setSessionOrder: changeSessionOrder,
    search,
    setSearch: changeSearch,
    enabledProviders,
    toggleProvider,
    sessions: displayedSessions.sessions,
    sessionChildren: displayedSessions.children,
    loadSessionChildren,
    retrySessionChildren,
    loadMoreSessionChildren,
    openRelatedSession,
    selectedSession,
    selectedSessionKey,
    selectSession,
    sessionsLoading,
    sessionsLoadingMore,
    sessionsError,
    sourceErrors,
    pendingProviders,
    sessionsCursor,
    retrySessions,
    loadMoreSessions,
    sessionIndexProgress,
    sessionIndexProgressLoading,
    sessionIndexProgressError,
    sessionIndexRetrying,
    retrySessionIndex,
    events: visibleEvents,
    outstandingQuestions: eventsAreOwned ? outstandingQuestions : [],
    questionNavigation: eventsAreOwned ? questionNavigation : null,
    openQuestion,
    selectQuestionSession,
    eventsOwnerKey: eventsAreOwned ? eventsOwnerKey : null,
    initialPageLoaded: initialPageSessionKey === selectedSessionKey && eventsAreOwned,
    selectedEvent,
    selectedEventKey: eventsAreOwned ? selectedEventKey : null,
    selectEvent,
    expandedEventKey: expandedEventIsVisible ? expandedEventKey : null,
    toggleEventExpanded,
    trajectoryPages,
    retryTrajectoryEvents,
    loadActivityGroup,
    setActivityGroupVisibility,
    expandedTrajectoryKey: expandedTrajectoryChild ? expandedTrajectoryEvent?.trajectory_key ?? null : null,
    expandedTrajectoryEventKey: expandedTrajectoryChild ? expandedTrajectoryEvent?.event_key ?? null : null,
    expandedActivityKeys,
    expandedActivities,
    expandedTrajectoryDetail,
    expandedTrajectoryDetailLoading,
    expandedTrajectoryDetailError,
    toggleTrajectoryEventExpanded,
    retryExpandedTrajectoryDetail,
    expandedDetail: expandedDetailIsOwned ? expandedDetail : null,
    expandedDetailLoading: expandedDetailTargetKey !== null
      && (!expandedDetailIsOwned || expandedDetailLoading),
    expandedDetailError: expandedDetailIsOwned ? expandedDetailError : null,
    retryExpandedDetail: () => setExpandedDetailAttempt((attempt) => attempt + 1),
    eventsLoading: selectedSessionKey !== null && (!eventsAreOwned || eventsLoading),
    olderLoading: eventsAreOwned && olderLoading,
    newerLoading: eventsAreOwned && newerLoading,
    eventsError: eventsAreOwned ? eventsError : null,
    followError: eventsAreOwned ? followError : null,
    olderCursor: eventsAreOwned ? olderCursor : null,
    newerCursor: eventsAreOwned ? newerCursor : null,
    totalEvents: eventsAreOwned ? totalEvents : null,
    historyStatus: eventsAreOwned ? historyStatus : null,
    retryEvents: () => setEventsAttempt((attempt) => attempt + 1),
    pendingLiveActivity,
    showLiveActivity,
    refreshSessionAfterInput,
    setFollowingLive,
    loadOlderEvents,
    loadNewerEvents,
    inspectorOpen,
    closeInspector,
    toggleInspector,
    mobileSidebarOpen,
    setMobileSidebarOpen,
    detail: detailIsOwned ? detail : null,
    detailLoading: inspectorOpen && detailTargetKey !== null && (!detailIsOwned || detailLoading),
    detailError: detailIsOwned ? detailError : null,
    retryDetail: () => setDetailAttempt((attempt) => attempt + 1),
  };
}
