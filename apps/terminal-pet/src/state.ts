import {
  asNumber,
  asObject,
  asString,
  type AgentEvent,
  type JsonObject,
  type RelayEvent,
  type RelaySession
} from "./protocol";

export const PET_STATES = [
  "idle",
  "running",
  "needs_input",
  "ready",
  "blocked"
] as const;

export type PetState = typeof PET_STATES[number];
export type PetOutcome = "completed" | "interrupted";

export type PetActivityKind =
  | "message"
  | "reasoning"
  | "tool"
  | "input"
  | "error"
  | "goal"
  | "agent"
  | "lifecycle"
  | "provider"
  | "activity";

export interface PetActivity {
  kind: PetActivityKind;
  label: string;
  detail?: string;
  at: number;
}

export interface PetPolicy {
  ready_debounce_ms: number;
  ready_hold_ms: number;
  recent_completion_ms: number;
  error_grace_ms: number;
  running_lease_ms: number;
  open_tool_lease_ms: number;
  blocked_lease_ms: number;
  input_lease_ms: number;
}

export interface PetFocus {
  topic: string;
  parent_topic?: string;
  root_topic: string;
  depth: number;
  is_provisional: boolean;
  state: PetState;
  family_state: PetState;
  family_last_event_at: number;
  state_changed_at: number;
  last_event_at: number;
  label: string;
  provider?: string;
  project_label?: string;
  title?: string;
  cwd?: string;
  session_id: string;
  agent?: string;
  current_activity?: PetActivity;
  completed_at?: number;
  recently_completed: boolean;
  outcome?: PetOutcome;
  descendant_count: number;
  active_descendant_count: number;
  urgent_descendant_count: number;
  recent_descendant_count: number;
}

export interface PetSnapshot {
  state: PetState;
  state_changed_at: number;
  active_sessions: number;
  total_sessions: number;
  sessions: PetFocus[];
  focus?: PetFocus;
}

export function effectivePetState(focus: PetFocus): PetState {
  return focus.depth === 0 ? focus.family_state : focus.state;
}

export function effectiveStateChangedAt(focus: PetFocus): number {
  return focus.depth === 0
    ? focus.family_last_event_at
    : focus.state_changed_at;
}

interface SessionActivity {
  topic: string;
  session: RelaySession;
  parent_topic?: string;
  is_provisional: boolean;
  last_event_at: number;
  label: string;
  current_activity?: PetActivity;
  outcome?: PetOutcome;
  running_until: number;
  ready_after?: number;
  ready_until?: number;
  completed_at?: number;
  blocked_after?: number;
  blocked_until?: number;
  open_tools: Map<string, number>;
  pending_interactions: Map<string, number>;
}

const DEFAULT_POLICY: PetPolicy = {
  ready_debounce_ms: 750,
  ready_hold_ms: 30_000,
  recent_completion_ms: 5 * 60_000,
  error_grace_ms: 1_500,
  running_lease_ms: 3 * 60_000,
  open_tool_lease_ms: 30 * 60_000,
  blocked_lease_ms: 60 * 60_000,
  input_lease_ms: 30 * 60_000
};

const INPUT_NATIVE_TYPES = new Set([
  "event_msg.exec_approval_request",
  "event_msg.apply_patch_approval_request",
  "event_msg.request_permissions",
  "event_msg.request_user_input",
  "event_msg.elicitation_request",
  "event_msg.model_verification"
]);

const RUNNING_NATIVE_TYPES = new Set([
  "event_msg.task_started",
  "event_msg.turn_started"
]);

const READY_NATIVE_TYPES = new Set([
  "event_msg.task_complete",
  "event_msg.turn_complete"
]);

const BLOCKED_NATIVE_TYPES = new Set([
  "event_msg.turn_aborted"
]);

const STATE_PRIORITY: Record<PetState, number> = {
  idle: 0,
  running: 1,
  ready: 2,
  blocked: 3,
  needs_input: 4
};

const MAX_FUTURE_EVENT_SKEW_MS = 5 * 60_000;

export class PetStore {
  readonly policy: PetPolicy;
  readonly sessions = new Map<string, SessionActivity>();
  readonly seen_agent_activity = new Set<string>();

  constructor(policy: Partial<PetPolicy> = {}) {
    this.policy = {
      ...DEFAULT_POLICY,
      ...policy
    };
  }

  ingest(relay: RelayEvent, nowMs = Date.now()): void {
    // Accounting and context updates are observations, not user/agent activity.
    // Ignoring them here also avoids creating a new focus row from metadata alone.
    if (
      relay.event.type === "usage"
      || relay.event.type === "compaction"
      || relay.event.type === "metadata"
      || asObject(relay.event.provenance)?.display === false
      || (
        relay.event.provider === "pi"
        && asObject(relay.event.native)?.type === "custom_message"
        && asObject(relay.event.native)?.display === false
      )
    ) {
      return;
    }
    const eventAt = eventOccurredAt(relay.event, nowMs);
    const activity = this.sessions.get(relay.topic)
      ?? newSessionActivity(relay, eventAt);
    activity.session = mergeSession(activity.session, relay.session);
    const resolvedParent = parentTopic(relay.topic, activity.session);
    activity.parent_topic = relay.session.parent_session_id === undefined
      ? resolvedParent ?? activity.parent_topic
      : resolvedParent;
    activity.is_provisional = false;
    this.#expireLeases(activity, nowMs);
    this.sessions.set(relay.topic, activity);
    this.#ensureParent(activity, eventAt);

    if (relay.event.type === "agent_activity") {
      if (!this.#isDuplicateAgentActivity(activity, relay.event)) {
        this.#applyAgentActivity(activity, relay.event, eventAt);
      }
      return;
    }

    if (eventAt < activity.last_event_at) {
      return;
    }
    const described = describeActivity(relay.event);
    recordActivity(
      activity,
      described.label,
      eventAt,
      described.kind,
      described.detail
    );
    this.#applyEvent(activity, relay.event, eventAt);
  }

  snapshot(nowMs = Date.now()): PetSnapshot {
    const candidates = new Map<string, PetFocus>();
    for (const activity of this.sessions.values()) {
      this.#expireLeases(activity, nowMs);
      candidates.set(activity.topic, this.#focusFor(activity, nowMs));
    }
    const lineages = resolveLineages(this.sessions);
    for (const candidate of candidates.values()) {
      const lineage = lineages.get(candidate.topic);
      candidate.parent_topic = lineage?.parent_topic;
      candidate.root_topic = lineage?.root_topic ?? candidate.topic;
      candidate.depth = lineage?.depth ?? 0;
    }
    applyFamilyAggregates(candidates);

    const allCandidates = [...candidates.values()];
    const active = allCandidates
      .filter((candidate) => candidate.state !== "idle")
      .sort(compareActiveSessions);
    const recent = allCandidates
      .filter((candidate) => candidate.state === "idle" && candidate.recently_completed)
      .sort(compareRecentCompletions);
    const visible = visibleFamilyTopics(candidates, [...active, ...recent]);
    const sessions = orderVisibleFamilies(candidates, visible);
    const focus = active[0] ?? recent[0];
    return {
      state: focus?.state ?? "idle",
      state_changed_at: focus?.state_changed_at ?? nowMs,
      active_sessions: active.length,
      total_sessions: allCandidates.length,
      sessions,
      focus
    };
  }

  acknowledge(topic: string): void {
    const activity = this.sessions.get(topic);
    if (!activity) {
      return;
    }
    activity.ready_after = undefined;
    activity.ready_until = undefined;
    activity.completed_at = undefined;
    activity.blocked_after = undefined;
    activity.blocked_until = undefined;
    activity.outcome = undefined;
  }

  #ensureParent(activity: SessionActivity, eventAt: number): void {
    const parent = activity.parent_topic;
    if (!parent || this.sessions.has(parent)) {
      return;
    }
    const provider = activity.session.provider ?? providerFromTopic(activity.topic);
    const sessionId = sessionIdFromTopic(parent);
    this.sessions.set(parent, {
      topic: parent,
      session: {
        provider,
        session_id: sessionId,
        agent_path: parentAgentPath(activity.session.agent_path),
        project: activity.session.project
      },
      is_provisional: true,
      last_event_at: eventAt,
      label: "Parent session",
      current_activity: {
        kind: "lifecycle",
        label: "Parent session",
        at: eventAt
      },
      running_until: eventAt,
      open_tools: new Map(),
      pending_interactions: new Map()
    });
  }

  #isDuplicateAgentActivity(activity: SessionActivity, event: AgentEvent): boolean {
    const eventId = asString(event.event_id);
    if (!eventId) {
      return false;
    }
    const provider = asString(event.provider)
      ?? activity.session.provider
      ?? providerFromTopic(activity.topic)
      ?? "unknown";
    const key = `${provider}:${eventId}`;
    if (this.seen_agent_activity.has(key)) {
      return true;
    }
    this.seen_agent_activity.add(key);
    return false;
  }

  #applyAgentActivity(source: SessionActivity, event: AgentEvent, eventAt: number): void {
    const targetSessionId = asString(event.target_session_id);
    const kind = asString(event.kind)?.toLowerCase();
    if (!kind) {
      return;
    }
    const provider = asString(event.provider)
      ?? source.session.provider
      ?? providerFromTopic(source.topic);
    if (!provider) {
      return;
    }
    if (!targetSessionId) {
      this.#applyPathAgentActivity(source, event, provider, kind, eventAt);
      return;
    }
    const targetTopic = `${provider}.${targetSessionId}`;
    const targetPath = asString(event.target_agent_path);
    const existing = this.sessions.get(targetTopic);
    const inferredParent = this.#inferParentTopic(
      source,
      targetPath,
      provider,
      asString(event.actor_agent_path)
    );

    if (
      kind === "started"
      && !existing
      && targetPath
      && inferredParent === undefined
    ) {
      return;
    }

    const target = existing ?? newProvisionalActivity(
      targetTopic,
      targetSessionId,
      provider,
      source,
      inferredParent,
      targetPath,
      eventAt
    );
    target.parent_topic ??= inferredParent;
    target.session.agent_path ??= targetPath;
    this.sessions.set(targetTopic, target);
    if (eventAt < target.last_event_at) {
      return;
    }
    if (kind !== "interacted" || target.outcome === undefined) {
      recordActivity(
        target,
        agentActivityLabel(kind),
        eventAt,
        "agent",
        agentActivityDetail(event)
      );
    }

    switch (kind) {
      case "started":
        this.#markProgress(target, eventAt);
        return;
      case "interacted":
        return;
      case "interrupted":
        this.#markInterrupted(target, eventAt);
        return;
      default:
        return;
    }
  }

  #applyPathAgentActivity(
    source: SessionActivity,
    event: AgentEvent,
    provider: string,
    kind: string,
    eventAt: number
  ): void {
    const targetAgentPath = asString(event.target_agent_path);
    if (targetAgentPath) {
      const matchingTargets = this
        .#familyCandidates(source, provider)
        .filter((candidate) => (
          effectiveAgentPath(candidate) === normalizeAgentPath(targetAgentPath)
        ));
      if (matchingTargets.length === 1) {
        recordActivity(
          matchingTargets[0]!,
          agentActivityLabel(kind),
          eventAt,
          "agent",
          agentActivityDetail(event)
        );
        return;
      }
    }

    const actorAgentPath = asString(event.actor_agent_path);
    if (
      actorAgentPath
      && normalizeAgentPath(actorAgentPath) === effectiveAgentPath(source)
    ) {
      recordActivity(
        source,
        agentActivityLabel(kind),
        eventAt,
        "agent",
        agentActivityDetail(event)
      );
    }
  }

  #inferParentTopic(
    source: SessionActivity,
    targetAgentPath: string | undefined,
    provider: string,
    actorAgentPath: string | undefined
  ): string | undefined {
    if (!targetAgentPath) {
      return source.topic;
    }

    const normalizedActorPath = actorAgentPath
      ? normalizeAgentPath(actorAgentPath)
      : undefined;
    const sourceAncestors = activityAncestorTopics(this.sessions, source.topic);
    let best: SessionActivity | undefined;
    let bestScore: readonly number[] | undefined;
    for (const candidate of this.#familyCandidates(source, provider)) {
      const candidatePath = effectiveAgentPath(candidate);
      if (!isDescendantAgentPath(candidatePath, targetAgentPath)) {
        continue;
      }
      const score = [
        Number(normalizedActorPath === candidatePath),
        candidatePath.length,
        Number(candidate.topic === source.topic),
        Number(sourceAncestors.has(candidate.topic))
      ];
      if (!bestScore || compareScores(score, bestScore) > 0) {
        best = candidate;
        bestScore = score;
      }
    }
    return best?.topic;
  }

  #familyCandidates(
    source: SessionActivity,
    provider: string
  ): SessionActivity[] {
    const sourceRoot = activityRootTopic(this.sessions, source.topic);
    return [...this.sessions.values()].filter((candidate) => (
      candidateProvider(candidate) === provider
      && activityRootTopic(this.sessions, candidate.topic) === sourceRoot
    ));
  }

  #applyEvent(activity: SessionActivity, event: AgentEvent, nowMs: number): void {
    switch (event.type) {
      case "session_started":
        activity.running_until = Math.max(activity.running_until, nowMs);
        activity.completed_at = undefined;
        activity.outcome = undefined;
        return;
      case "message":
        this.#applyMessage(activity, event, nowMs);
        return;
      case "reasoning":
        this.#markProgress(activity, nowMs);
        return;
      case "tool_call":
        this.#applyToolCall(activity, event, nowMs);
        return;
      case "question_request":
        if (event.is_blocking !== false) {
          this.#markNeedsInput(activity, event, nowMs, "question_request");
        } else {
          this.#markProgress(activity, nowMs);
        }
        return;
      case "question_reply":
        activity.pending_interactions.delete(asString(event.request_id) ?? "question_request");
        this.#markProgress(activity, nowMs);
        return;
      case "error":
        if (isInterruptedError(event)) {
          this.#markInterrupted(activity, nowMs);
          return;
        }
        activity.ready_after = undefined;
        activity.ready_until = undefined;
        activity.completed_at = undefined;
        activity.outcome = undefined;
        activity.blocked_after = nowMs + this.policy.error_grace_ms;
        activity.blocked_until = activity.blocked_after + this.policy.blocked_lease_ms;
        return;
      case "goal_updated":
        this.#applyGoal(activity, event, nowMs);
        return;
      case "agent_activity":
        return;
      case "unknown":
        this.#applyUnknown(activity, event, nowMs);
        return;
      case "provider_changed":
      case "session_settings_applied":
        return;
      default:
        return;
    }
  }

  #applyMessage(activity: SessionActivity, event: AgentEvent, nowMs: number): void {
    const role = asString(event.role);
    const delivery = asString(event.delivery);
    const phase = asString(event.phase);
    if (role === "assistant" && delivery !== "commentary" && phase === "finished") {
      activity.blocked_after = undefined;
      activity.blocked_until = undefined;
      activity.ready_after = nowMs + this.policy.ready_debounce_ms;
      activity.ready_until = activity.ready_after + this.policy.ready_hold_ms;
      activity.completed_at = undefined;
      activity.outcome = undefined;
      activity.running_until = activity.ready_after;
      return;
    }

    this.#markProgress(activity, nowMs);
    if (role === "user") {
      activity.open_tools.clear();
      activity.pending_interactions.clear();
    }
  }

  #applyToolCall(activity: SessionActivity, event: AgentEvent, nowMs: number): void {
    const toolName = asString(event.tool_name);
    const toolCallId = asString(event.tool_call_id);
    const phase = asString(event.phase);
    const hasInput = event.input !== undefined && event.input !== null;
    const hasOutput = event.output !== undefined && event.output !== null;

    if (toolName && isInputTool(toolName) && !hasOutput) {
      this.#markNeedsInput(activity, event, nowMs, `tool:${toolName}`);
      return;
    }

    this.#markProgress(activity, nowMs);
    if (!toolCallId) {
      return;
    }

    if (hasOutput) {
      activity.open_tools.delete(toolCallId);
      activity.pending_interactions.delete(toolCallId);
    } else if (phase === "started" || phase === "delta" || phase === "updated" || hasInput) {
      activity.open_tools.set(toolCallId, nowMs + this.policy.open_tool_lease_ms);
    } else if (phase === "finished") {
      activity.open_tools.delete(toolCallId);
    }
  }

  #applyGoal(activity: SessionActivity, event: AgentEvent, nowMs: number): void {
    const goal = asObject(event.goal);
    const status = asString(goal?.status)?.toLowerCase();
    switch (status) {
      case "complete":
      case "completed":
        this.#markReady(activity, nowMs);
        return;
      case "blocked":
      case "budget_limited":
      case "usage_limited":
        this.#markBlocked(activity, nowMs);
        return;
      case "paused":
        this.#clearTransientState(activity, nowMs);
        return;
      case "active":
      case "in_progress":
        this.#markProgress(activity, nowMs);
        return;
      default:
        this.#markProgress(activity, nowMs);
    }
  }

  #applyUnknown(activity: SessionActivity, event: AgentEvent, nowMs: number): void {
    const nativeType = asString(event.native_type)?.toLowerCase();
    if (!nativeType) {
      return;
    }
    if (INPUT_NATIVE_TYPES.has(nativeType)) {
      this.#markNeedsInput(activity, event, nowMs, nativeType);
    } else if (RUNNING_NATIVE_TYPES.has(nativeType)) {
      this.#markProgress(activity, nowMs);
    } else if (READY_NATIVE_TYPES.has(nativeType)) {
      this.#markReady(activity, nowMs);
    } else if (BLOCKED_NATIVE_TYPES.has(nativeType)) {
      this.#markBlocked(activity, nowMs);
    }
  }

  #markProgress(activity: SessionActivity, nowMs: number): void {
    activity.running_until = nowMs + this.policy.running_lease_ms;
    activity.ready_after = undefined;
    activity.ready_until = undefined;
    activity.completed_at = undefined;
    activity.outcome = undefined;
    activity.blocked_after = undefined;
    activity.blocked_until = undefined;
  }

  #markReady(activity: SessionActivity, nowMs: number): void {
    activity.open_tools.clear();
    activity.pending_interactions.clear();
    activity.blocked_after = undefined;
    activity.blocked_until = undefined;
    activity.ready_after = nowMs;
    activity.ready_until = nowMs + this.policy.ready_hold_ms;
    activity.completed_at = undefined;
    activity.outcome = undefined;
    activity.running_until = nowMs;
  }

  #markBlocked(activity: SessionActivity, nowMs: number): void {
    activity.ready_after = undefined;
    activity.ready_until = undefined;
    activity.completed_at = undefined;
    activity.outcome = undefined;
    activity.blocked_after = nowMs;
    activity.blocked_until = nowMs + this.policy.blocked_lease_ms;
    activity.running_until = nowMs;
  }

  #markNeedsInput(
    activity: SessionActivity,
    event: AgentEvent,
    nowMs: number,
    fallbackKey: string
  ): void {
    const native = asObject(event.native);
    const interactionKey = [
      native?.approval_id,
      native?.call_id,
      native?.id,
      native?.turn_id,
      event.tool_call_id,
      event.request_id
    ].map(asString).find(Boolean) ?? fallbackKey;
    const autoResolutionMs = asNumber(native?.autoResolutionMs)
      ?? asNumber(native?.auto_resolution_ms);
    const lease = autoResolutionMs === undefined
      ? this.policy.input_lease_ms
      : autoResolutionMs + 2_000;

    activity.pending_interactions.set(interactionKey, nowMs + lease);
    activity.ready_after = undefined;
    activity.ready_until = undefined;
    activity.completed_at = undefined;
    activity.outcome = undefined;
    activity.blocked_after = undefined;
    activity.blocked_until = undefined;
    activity.running_until = Math.max(activity.running_until, nowMs);
  }

  #clearTransientState(activity: SessionActivity, nowMs: number): void {
    activity.open_tools.clear();
    activity.pending_interactions.clear();
    activity.ready_after = undefined;
    activity.ready_until = undefined;
    activity.completed_at = undefined;
    activity.outcome = undefined;
    activity.blocked_after = undefined;
    activity.blocked_until = undefined;
    activity.running_until = nowMs;
  }

  #markInterrupted(activity: SessionActivity, nowMs: number): void {
    this.#clearTransientState(activity, nowMs);
    activity.completed_at = nowMs;
    activity.outcome = "interrupted";
    activity.label = "Interrupted";
  }

  #expireLeases(activity: SessionActivity, nowMs: number): void {
    for (const [key, expiresAt] of activity.open_tools) {
      if (expiresAt <= nowMs) {
        activity.open_tools.delete(key);
      }
    }
    for (const [key, expiresAt] of activity.pending_interactions) {
      if (expiresAt <= nowMs) {
        activity.pending_interactions.delete(key);
      }
    }
    if (
      activity.completed_at === undefined
      && activity.ready_after !== undefined
      && activity.ready_after <= nowMs
      && activity.open_tools.size === 0
      && activity.pending_interactions.size === 0
    ) {
      activity.completed_at = activity.ready_after;
      activity.outcome ??= "completed";
    }
    if (activity.ready_until !== undefined && activity.ready_until <= nowMs) {
      activity.ready_after = undefined;
      activity.ready_until = undefined;
    }
    if (activity.blocked_until !== undefined && activity.blocked_until <= nowMs) {
      activity.blocked_after = undefined;
      activity.blocked_until = undefined;
    }
  }

  #focusFor(activity: SessionActivity, nowMs: number): PetFocus {
    let state: PetState = "idle";
    let changedAt = activity.last_event_at;

    if (activity.pending_interactions.size > 0) {
      state = "needs_input";
    } else if (
      activity.blocked_after !== undefined
      && activity.blocked_until !== undefined
      && activity.blocked_after <= nowMs
      && nowMs < activity.blocked_until
    ) {
      state = "blocked";
      changedAt = activity.blocked_after;
    } else if (
      activity.ready_after !== undefined
      && activity.ready_until !== undefined
      && activity.open_tools.size === 0
      && activity.ready_after <= nowMs
      && nowMs < activity.ready_until
    ) {
      state = "ready";
      changedAt = activity.ready_after;
    } else if (activity.open_tools.size > 0 || activity.running_until > nowMs) {
      state = "running";
    }
    const recentlyCompleted = activity.completed_at !== undefined
      && nowMs < activity.completed_at + this.policy.recent_completion_ms;

    return {
      topic: activity.topic,
      parent_topic: activity.parent_topic,
      root_topic: activity.topic,
      depth: 0,
      is_provisional: activity.is_provisional,
      state,
      family_state: state,
      family_last_event_at: activity.last_event_at,
      state_changed_at: changedAt,
      last_event_at: activity.last_event_at,
      label: activity.label,
      provider: activity.session.provider,
      project_label: displayProjectName(activity.session.project),
      title: activity.session.title ?? undefined,
      cwd: activity.session.cwd ?? undefined,
      session_id: activity.session.session_id,
      agent: activity.session.agent_nickname
        ?? activity.session.agent_path
        ?? undefined,
      current_activity: activity.current_activity,
      completed_at: activity.completed_at,
      recently_completed: recentlyCompleted,
      outcome: activity.outcome,
      descendant_count: 0,
      active_descendant_count: 0,
      urgent_descendant_count: 0,
      recent_descendant_count: 0
    };
  }
}

function displayProjectName(project: RelaySession["project"]): string | undefined {
  return project?.project_name
    ?? project?.folder_name
    ?? project?.repository_name
    ?? project?.name
    ?? undefined;
}

function compareActiveSessions(left: PetFocus, right: PetFocus): number {
  const priority = STATE_PRIORITY[right.state] - STATE_PRIORITY[left.state];
  if (priority !== 0) {
    return priority;
  }
  const stateRecency = right.state_changed_at - left.state_changed_at;
  if (stateRecency !== 0) {
    return stateRecency;
  }
  const eventRecency = right.last_event_at - left.last_event_at;
  return eventRecency !== 0 ? eventRecency : compareTopics(left.topic, right.topic);
}

function compareRecentCompletions(left: PetFocus, right: PetFocus): number {
  const recency = (right.completed_at ?? 0) - (left.completed_at ?? 0);
  return recency !== 0 ? recency : compareTopics(left.topic, right.topic);
}

function compareTopics(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

interface SessionLineage {
  parent_topic?: string;
  root_topic: string;
  depth: number;
}

function resolveLineages(
  sessions: Map<string, SessionActivity>
): Map<string, SessionLineage> {
  const resolved = new Map<string, SessionLineage>();

  function resolve(topic: string, visiting: Set<string>): SessionLineage {
    const cached = resolved.get(topic);
    if (cached) {
      return cached;
    }
    const activity = sessions.get(topic);
    const parent = activity?.parent_topic;
    if (
      !parent
      || parent === topic
      || !sessions.has(parent)
      || visiting.has(parent)
    ) {
      const lineage = {
        root_topic: topic,
        depth: 0
      };
      resolved.set(topic, lineage);
      return lineage;
    }
    visiting.add(topic);
    const parentLineage = resolve(parent, visiting);
    visiting.delete(topic);
    const lineage = {
      parent_topic: parent,
      root_topic: parentLineage.root_topic,
      depth: parentLineage.depth + 1
    };
    resolved.set(topic, lineage);
    return lineage;
  }

  for (const topic of sessions.keys()) {
    resolve(topic, new Set());
  }
  return resolved;
}

function applyFamilyAggregates(candidates: Map<string, PetFocus>): void {
  const children = childTopics(candidates);
  const visited = new Set<string>();

  function aggregate(topic: string): PetFocus | undefined {
    const focus = candidates.get(topic);
    if (!focus || visited.has(topic)) {
      return focus;
    }
    visited.add(topic);
    for (const childTopic of children.get(topic) ?? []) {
      const child = aggregate(childTopic);
      if (!child) {
        continue;
      }
      focus.descendant_count += 1 + child.descendant_count;
      focus.active_descendant_count += Number(child.state !== "idle")
        + child.active_descendant_count;
      focus.urgent_descendant_count += Number(isUrgentState(child.state))
        + child.urgent_descendant_count;
      focus.recent_descendant_count += Number(
        child.state === "idle" && child.recently_completed
      ) + child.recent_descendant_count;
      if (STATE_PRIORITY[child.family_state] > STATE_PRIORITY[focus.family_state]) {
        focus.family_state = child.family_state;
      }
      focus.family_last_event_at = Math.max(
        focus.family_last_event_at,
        child.family_last_event_at
      );
    }
    return focus;
  }

  for (const focus of candidates.values()) {
    if (focus.parent_topic === undefined) {
      aggregate(focus.topic);
    }
  }
  for (const focus of candidates.values()) {
    aggregate(focus.topic);
  }
}

function visibleFamilyTopics(
  candidates: Map<string, PetFocus>,
  visibleNodes: PetFocus[]
): Set<string> {
  const visible = new Set<string>();
  for (const node of visibleNodes) {
    let current: PetFocus | undefined = node;
    const visited = new Set<string>();
    while (current && !visited.has(current.topic)) {
      visited.add(current.topic);
      visible.add(current.topic);
      current = current.parent_topic
        ? candidates.get(current.parent_topic)
        : undefined;
    }
  }
  return visible;
}

function orderVisibleFamilies(
  candidates: Map<string, PetFocus>,
  visible: Set<string>
): PetFocus[] {
  if (visible.size === 0) {
    return [];
  }
  const children = childTopics(candidates);
  const roots = [...visible]
    .map((topic) => candidates.get(topic))
    .filter((focus): focus is PetFocus => (
      focus !== undefined
      && (focus.parent_topic === undefined || !visible.has(focus.parent_topic))
    ))
    .sort(compareFamilyRoots);
  const ordered: PetFocus[] = [];

  function append(topic: string): void {
    const focus = candidates.get(topic);
    if (!focus || !visible.has(topic)) {
      return;
    }
    ordered.push(focus);
    const visibleChildren = (children.get(topic) ?? [])
      .map((childTopic) => candidates.get(childTopic))
      .filter((child): child is PetFocus => child !== undefined && visible.has(child.topic))
      .sort(compareFamilySiblings);
    for (const child of visibleChildren) {
      append(child.topic);
    }
  }

  for (const root of roots) {
    append(root.topic);
  }
  return ordered;
}

function childTopics(candidates: Map<string, PetFocus>): Map<string, string[]> {
  const children = new Map<string, string[]>();
  for (const focus of candidates.values()) {
    if (!focus.parent_topic || !candidates.has(focus.parent_topic)) {
      continue;
    }
    const topics = children.get(focus.parent_topic) ?? [];
    topics.push(focus.topic);
    children.set(focus.parent_topic, topics);
  }
  return children;
}

function compareFamilyRoots(
  left: PetFocus,
  right: PetFocus
): number {
  const priority = STATE_PRIORITY[right.family_state]
    - STATE_PRIORITY[left.family_state];
  if (priority !== 0) {
    return priority;
  }
  const activity = right.family_last_event_at - left.family_last_event_at;
  return activity !== 0 ? activity : compareTopics(left.topic, right.topic);
}

function compareFamilySiblings(left: PetFocus, right: PetFocus): number {
  const familyPriority = STATE_PRIORITY[right.family_state]
    - STATE_PRIORITY[left.family_state];
  if (familyPriority !== 0) {
    return familyPriority;
  }
  const leftVisibleState = visibleStatePriority(left);
  const rightVisibleState = visibleStatePriority(right);
  if (leftVisibleState !== rightVisibleState) {
    return rightVisibleState - leftVisibleState;
  }
  if (left.state !== "idle" && right.state !== "idle") {
    return compareActiveSessions(left, right);
  }
  if (left.recently_completed && right.recently_completed) {
    return compareRecentCompletions(left, right);
  }
  const recency = right.last_event_at - left.last_event_at;
  return recency !== 0 ? recency : compareTopics(left.topic, right.topic);
}

function visibleStatePriority(focus: PetFocus): number {
  if (focus.state !== "idle") {
    return 2;
  }
  if (focus.recently_completed) {
    return 1;
  }
  return 0;
}

function isUrgentState(state: PetState): boolean {
  return state === "needs_input" || state === "blocked";
}

function isInterruptedError(event: AgentEvent): boolean {
  return asString(event.provider)?.toLowerCase() === "codex"
    && asString(event.message)?.trim().toLowerCase() === "interrupted";
}

function newSessionActivity(relay: RelayEvent, nowMs: number): SessionActivity {
  return {
    topic: relay.topic,
    session: relay.session,
    parent_topic: parentTopic(relay.topic, relay.session),
    is_provisional: false,
    last_event_at: nowMs,
    label: relay.event.type === "agent_activity"
      ? "Session observed"
      : describeEvent(relay.event),
    running_until: nowMs,
    open_tools: new Map(),
    pending_interactions: new Map()
  };
}

function newProvisionalActivity(
  topic: string,
  sessionId: string,
  provider: string,
  source: SessionActivity,
  parent: string | undefined,
  agentPath: string | undefined,
  eventAt: number
): SessionActivity {
  return {
    topic,
    parent_topic: parent,
    is_provisional: true,
    session: {
      provider,
      session_id: sessionId,
      parent_session_id: parent ? sessionIdFromTopic(parent) : undefined,
      agent_path: agentPath,
      cwd: source.session.cwd,
      project: source.session.project
    },
    last_event_at: eventAt,
    label: "Agent activity",
    current_activity: {
      kind: "agent",
      label: "Agent activity",
      at: eventAt
    },
    running_until: eventAt,
    open_tools: new Map(),
    pending_interactions: new Map()
  };
}

function mergeSession(
  previous: RelaySession,
  incoming: RelaySession
): RelaySession {
  return {
    ...previous,
    ...incoming,
    provider: incoming.provider ?? previous.provider,
    parent_session_id: incoming.parent_session_id === undefined
      ? previous.parent_session_id
      : incoming.parent_session_id,
    agent_path: incoming.agent_path ?? previous.agent_path,
    agent_nickname: incoming.agent_nickname ?? previous.agent_nickname,
    agent_role: incoming.agent_role ?? previous.agent_role,
    title: incoming.title ?? previous.title,
    cwd: incoming.cwd ?? previous.cwd,
    project: incoming.project ?? previous.project
  };
}

function parentTopic(
  topic: string,
  session: RelaySession
): string | undefined {
  const parentSessionId = session.parent_session_id ?? undefined;
  const provider = session.provider ?? providerFromTopic(topic);
  return parentSessionId && provider
    ? `${provider}.${parentSessionId}`
    : undefined;
}

function providerFromTopic(topic: string): string | undefined {
  const separator = topic.indexOf(".");
  return separator > 0 ? topic.slice(0, separator) : undefined;
}

function sessionIdFromTopic(topic: string): string {
  const separator = topic.indexOf(".");
  return separator >= 0 ? topic.slice(separator + 1) : topic;
}

function parentAgentPath(agentPath: string | null | undefined): string | undefined {
  if (!agentPath) {
    return undefined;
  }
  const normalized = agentPath.replace(/\/+$/, "");
  const separator = normalized.lastIndexOf("/");
  if (separator <= 0) {
    return undefined;
  }
  return normalized.slice(0, separator) || "/";
}

function effectiveAgentPath(activity: SessionActivity): string {
  const agentPath = activity.session.agent_path;
  if (agentPath) {
    return normalizeAgentPath(agentPath);
  }
  return activity.parent_topic === undefined ? "/root" : "";
}

function isDescendantAgentPath(parent: string, target: string): boolean {
  const normalizedParent = normalizeAgentPath(parent);
  const normalizedTarget = normalizeAgentPath(target);
  return normalizedParent.length > 0
    && normalizedTarget.startsWith(`${normalizedParent}/`);
}

function candidateProvider(activity: SessionActivity): string | undefined {
  return activity.session.provider ?? providerFromTopic(activity.topic);
}

function activityRootTopic(
  sessions: Map<string, SessionActivity>,
  topic: string
): string {
  let current = topic;
  const visited = new Set<string>();
  while (!visited.has(current)) {
    visited.add(current);
    const parent = sessions.get(current)?.parent_topic;
    if (!parent || !sessions.has(parent)) {
      return current;
    }
    current = parent;
  }
  return topic;
}

function activityAncestorTopics(
  sessions: Map<string, SessionActivity>,
  topic: string
): Set<string> {
  const ancestors = new Set<string>();
  let current: string | undefined = topic;
  while (current && !ancestors.has(current)) {
    ancestors.add(current);
    current = sessions.get(current)?.parent_topic;
  }
  return ancestors;
}

function compareScores(
  left: readonly number[],
  right: readonly number[]
): number {
  const length = Math.max(left.length, right.length);
  for (let index = 0; index < length; index += 1) {
    const difference = (left[index] ?? 0) - (right[index] ?? 0);
    if (difference !== 0) {
      return difference;
    }
  }
  return 0;
}

function normalizeAgentPath(path: string): string {
  return path.replace(/\/+$/, "") || "/";
}

function agentActivityLabel(kind: string): string {
  switch (kind) {
    case "started":
      return "Agent started";
    case "interacted":
      return "Agent interaction";
    case "interrupted":
      return "Interrupted";
    default:
      return `Agent ${kind}`;
  }
}

function agentActivityDetail(event: AgentEvent): string | undefined {
  const target = asString(event.target_agent_path)
    ?? asString(event.target_session_id);
  const actor = asString(event.actor_agent_path);
  if (target && actor) {
    return `${actor} → ${target}`;
  }
  return target ?? actor;
}

function recordActivity(
  activity: SessionActivity,
  label: string,
  eventAt: number,
  kind: PetActivityKind = "activity",
  detail?: string
): void {
  if (eventAt < activity.last_event_at) {
    return;
  }
  activity.last_event_at = eventAt;
  activity.label = label;
  activity.current_activity = {
    kind,
    label,
    detail,
    at: eventAt
  };
}

function eventOccurredAt(event: AgentEvent, receivedAt: number): number {
  const occurredAt = asNumber(event.occurred_at_ms);
  const timestamp = asString(event.timestamp);
  const timestampAt = timestamp ? Date.parse(timestamp) : Number.NaN;
  for (const candidate of [occurredAt, timestampAt]) {
    if (
      candidate !== undefined
      && Number.isFinite(candidate)
      && candidate >= 0
      && candidate <= receivedAt + MAX_FUTURE_EVENT_SKEW_MS
    ) {
      return candidate;
    }
  }
  return receivedAt;
}

export function describeEvent(event: AgentEvent): string {
  switch (event.type) {
    case "message": {
      const role = asString(event.role) ?? "message";
      const text = asString(event.text);
      return text ? `${role}: ${firstLine(text)}` : role;
    }
    case "reasoning":
      return "Thinking";
    case "tool_call":
      return describeTool(event);
    case "error":
      return asString(event.message) ?? "Blocked";
    case "goal_updated":
      return "Goal updated";
    case "agent_activity": {
      const kind = asString(event.kind) ?? "activity";
      const target = asString(event.target_agent_path);
      return target ? `${kind} ${target}` : kind;
    }
    case "unknown":
      return asString(event.native_type) ?? "Provider activity";
    case "session_started":
      return "Session started";
    case "provider_changed":
      return "Provider changed";
    case "session_settings_applied":
      return "Settings applied";
    default:
      return event.type.replaceAll("_", " ");
  }
}

interface ActivityDescription {
  kind: PetActivityKind;
  label: string;
  detail?: string;
}

function describeActivity(event: AgentEvent): ActivityDescription {
  const label = describeEvent(event);
  switch (event.type) {
    case "message":
      return {
        kind: "message",
        label,
        detail: firstLine(asString(event.text) ?? "") || undefined
      };
    case "reasoning":
      return {
        kind: "reasoning",
        label,
        detail: firstLine(asString(event.text) ?? "") || undefined
      };
    case "tool_call":
      return {
        kind: "tool",
        label,
        detail: toolDetail(event)
      };
    case "error":
      return {
        kind: "error",
        label,
        detail: asString(event.message)
      };
    case "goal_updated":
      return {
        kind: "goal",
        label,
        detail: goalDetail(event)
      };
    case "agent_activity":
      return {
        kind: "agent",
        label,
        detail: agentActivityDetail(event)
      };
    case "unknown": {
      const nativeType = asString(event.native_type)?.toLowerCase();
      return {
        kind: nativeType && INPUT_NATIVE_TYPES.has(nativeType)
          ? "input"
          : "provider",
        label,
        detail: unknownDetail(event)
      };
    }
    case "session_started":
      return { kind: "lifecycle", label };
    case "provider_changed":
    case "session_settings_applied":
      return { kind: "provider", label };
    default:
      return { kind: "activity", label };
  }
}

function describeTool(event: AgentEvent): string {
  const summary = asObject(event.summary);
  const summaryType = asString(summary?.type);
  const detail = toolDetail(event);
  const name = asString(event.tool_name) ?? summaryType ?? "tool";
  return detail ? `${name}: ${firstLine(detail)}` : name;
}

function toolDetail(event: AgentEvent): string | undefined {
  const summary = asObject(event.summary);
  const input = asObject(event.input);
  return firstString([
    summary?.path,
    summary?.command,
    summary?.query,
    summary?.url,
    summary?.title,
    input?.path,
    input?.command,
    input?.cmd,
    input?.query,
    input?.prompt,
    event.input
  ]);
}

function goalDetail(event: AgentEvent): string | undefined {
  const goal = asObject(event.goal);
  return firstString([
    goal?.status,
    goal?.title,
    goal?.description,
    goal?.name
  ]);
}

function unknownDetail(event: AgentEvent): string | undefined {
  const native = asObject(event.native);
  return firstString([
    native?.prompt,
    native?.question,
    native?.message,
    native?.description,
    native?.command,
    native?.reason,
    native?.title
  ]);
}

function firstString(values: unknown[]): string | undefined {
  return values.map(asString).find(Boolean);
}

function firstLine(value: string): string {
  return value.split(/\r?\n/, 1)[0]?.trim() ?? value;
}

function isInputTool(name: string): boolean {
  const normalized = name.split(".").at(-1)?.toLowerCase();
  return normalized === "request_user_input"
    || normalized === "ask_user_question"
    || normalized === "elicitation_request";
}
