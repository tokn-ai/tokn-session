import { useMemo, type RefObject } from "react";
import { PROVIDERS } from "../lib/types";
import type {
  SessionIndexBodyProviderProgress,
  SessionIndexProgress,
  SessionIndexWorkerError,
  SourceError,
  ViewerProvider,
} from "../lib/types";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { WarningIcon } from "./Icons";

const PROVIDER_LABELS: Record<ViewerProvider, string> = {
  codex: "Codex",
  pi: "Pi",
  opencode: "OpenCode",
  zcode: "ZCode",
  workbuddy: "WorkBuddy",
  dsh: "DeepSeek Harness",
};

export type IndexStatusTone = "neutral" | "active" | "warning";

export interface IndexStatusPresentation {
  label: string;
  detail: string;
  tone: IndexStatusTone;
}

function providerLabel(provider: ViewerProvider | null): string | null {
  return provider ? PROVIDER_LABELS[provider] : null;
}

type CatalogStatusLabel = "Finding sessions" | "Checking for changes";

/**
 * A provider without its first durable catalog still needs a full discovery
 * pass. This also makes a missing field from an older hot-reloaded backend
 * safely read as the established full-scan behavior.
 */
export function isTargetedCatalog(progress: SessionIndexProgress | null): boolean {
  return progress?.catalog.scope === "targeted"
    && progress.catalog.pending_providers.length === 0;
}

function catalogStatusLabel(progress: SessionIndexProgress | null): CatalogStatusLabel {
  return isTargetedCatalog(progress) ? "Checking for changes" : "Finding sessions";
}

function catalogDetail(
  progress: SessionIndexProgress | null,
  provider: ViewerProvider | null,
  remaining = false,
): string {
  const providerName = providerLabel(provider);
  if (isTargetedCatalog(progress)) {
    if (providerName) {
      return `${remaining ? "Checking remaining saved" : "Checking saved"} ${providerName} sessions for changes.`;
    }
    return "Checking saved sessions for changes.";
  }
  if (providerName) {
    return `${remaining ? "Looking for remaining saved" : "Looking for saved"} ${providerName} sessions.`;
  }
  return "Looking for saved sessions.";
}

function catalogSummaryLabel(progress: SessionIndexProgress): string {
  return `${catalogStatusLabel(progress)} · ${progress.catalog.processed_providers} / ${progress.catalog.total_providers} providers`;
}

function hasIndexFailures(progress: SessionIndexProgress | null, sourceErrors: SourceError[]): boolean {
  return sourceErrors.length > 0
    || (progress?.worker_error ?? null) !== null
    || (progress?.catalog.error_providers.length ?? 0) > 0
    || (progress?.body.failed_jobs ?? 0) > 0;
}

function workerErrorMessage(workerError: SessionIndexWorkerError): string {
  switch (workerError) {
    case "refresh_failed":
      return "The session index refresh could not continue. Retry indexing to start another pass.";
    case "task_failed":
      return "The background session index task stopped unexpectedly. Retry indexing to start another pass.";
  }
}

interface ProviderIndexStatus {
  provider: ViewerProvider;
  label: CatalogStatusLabel | "Loading details" | "Queued" | "Up to date" | "Needs attention";
  display_label: string;
  detail: string;
  tone: "active" | "neutral" | "warning";
}

function detailProgress(body: SessionIndexBodyProviderProgress | undefined): string {
  return `${body?.completed_jobs ?? 0} / ${body?.total_jobs ?? 0}`;
}

function providerStatuses(
  progress: SessionIndexProgress | null,
  sourceErrors: SourceError[],
): ProviderIndexStatus[] {
  const sourceErrorProviders = new Set(sourceErrors.map((error) => error.provider));
  const bodyProgress = new Map(
    (progress?.body.providers ?? []).map((provider) => [provider.provider, provider]),
  );

  return PROVIDERS.map((provider) => {
    const body = bodyProgress.get(provider);
    const sourceError = sourceErrors.find((error) => error.provider === provider);
    const hasFailure = sourceErrorProviders.has(provider)
      || (progress?.catalog.error_providers.includes(provider) ?? false)
      || (body?.failed_jobs ?? 0) > 0;
    const failureDetail = sourceError?.message
      ?? "A previous indexing attempt needs attention; retry indexing to try it again.";
    const isActiveCatalogProvider = progress?.catalog.active_provider === provider;
    if (isActiveCatalogProvider) {
      const label = catalogStatusLabel(progress);
      return {
        provider,
        label,
        display_label: label,
        detail: hasFailure
          ? `${failureDetail} ${isTargetedCatalog(progress)
            ? catalogDetail(progress, provider, true)
            : "Looking for remaining saved sessions."}`
          : isTargetedCatalog(progress)
            ? catalogDetail(progress, provider)
            : "Looking for saved sessions.",
        tone: hasFailure ? "warning" : "active",
      };
    }
    const isActiveBodyProvider = progress?.body.active_provider === provider;
    if (isActiveBodyProvider) {
      return {
        provider,
        label: "Loading details",
        display_label: `Loading details · ${detailProgress(body)}`,
        detail: hasFailure
          ? `${failureDetail} Loading remaining saved session details.`
          : "Loading saved session details.",
        tone: hasFailure ? "warning" : "active",
      };
    }
    if (hasFailure) {
      return {
        provider,
        label: "Needs attention",
        display_label: "Needs attention",
        detail: failureDetail,
        tone: "warning",
      };
    }
    if (progress?.catalog.pending_providers.includes(provider)) {
      return {
        provider,
        label: "Queued",
        display_label: "Queued",
        detail: isTargetedCatalog(progress)
          ? "Waiting to check saved sessions for changes."
          : "Waiting to find sessions.",
        tone: "neutral",
      };
    }
    if ((body?.pending_jobs ?? 0) > 0) {
      return {
        provider,
        label: "Queued",
        display_label: `Queued · ${detailProgress(body)}`,
        detail: "Waiting to load session details.",
        tone: "neutral",
      };
    }
    const hasKnownDetailJobs = (body?.total_jobs ?? 0) > 0;
    return {
      provider,
      label: "Up to date",
      display_label: hasKnownDetailJobs ? `Up to date · ${detailProgress(body)}` : "Up to date",
      detail: progress ? "Session details are current." : "Waiting for index status.",
      tone: "neutral",
    };
  });
}

/** A compact, shared summary for the status bar and notification heading. */
export function describeSessionIndexProgress(
  progress: SessionIndexProgress | null,
  isLoading: boolean,
  error: string | null,
  sourceErrors: SourceError[],
): IndexStatusPresentation {
  // A retry command can fail while we still have a useful last-known
  // snapshot. Prefer that failure over stale "up to date" or queue wording so
  // the Retry button never appears to have succeeded when it did not.
  if (error) {
    return {
      label: "Session index needs attention",
      detail: error,
      tone: "warning",
    };
  }

  if (!progress) {
    return {
      label: isLoading ? "Checking session index…" : "Session index status unavailable",
      detail: "Open notifications for indexing details.",
      tone: "neutral",
    };
  }

  if (progress.activity === "waiting_for_indexer") {
    return {
      label: "Using shared session index",
      detail: "Another viewer process owns indexing. This viewer reads its saved updates.",
      tone: "neutral",
    };
  }

  if (progress.worker_error) {
    return {
      label: "Session index worker needs attention · retry scheduled",
      detail: "The scheduler needs another attempt before indexing can continue.",
      tone: "warning",
    };
  }

  const failures = hasIndexFailures(progress, sourceErrors);
  const tone: IndexStatusTone = failures ? "warning" : progress.is_refreshing ? "active" : "neutral";
  const activeCatalogProvider = providerLabel(progress.catalog.active_provider);
  const activeBodyProvider = providerLabel(progress.body.active_provider);

  // The first durable catalog pass can publish a pending-provider snapshot a
  // moment before the scheduler changes its activity enum. Do not falsely say
  // the index is current during that small but visible window.
  if (progress.catalog.pending_providers.length > 0) {
    return {
      label: catalogSummaryLabel(progress),
      detail: activeCatalogProvider
        ? catalogDetail(progress, progress.catalog.active_provider)
        : `${progress.catalog.pending_providers.length} provider${progress.catalog.pending_providers.length === 1 ? " is" : "s are"} queued.`,
      tone: failures ? "warning" : "active",
    };
  }
  if (progress.body.pending_jobs > 0 && progress.activity === "idle") {
    return {
      label: `Details queued · ${progress.body.pending_jobs} remaining`,
      detail: "Session details are waiting to be loaded.",
      tone,
    };
  }
  if (progress.body.pending_jobs > 0 && progress.activity === "waiting_to_retry") {
    return {
      label: `Details queued · ${progress.body.pending_jobs} remaining`,
      detail: "The next batch is scheduled.",
      tone: failures ? "warning" : "neutral",
    };
  }

  switch (progress.activity) {
    case "catalog":
      return {
        label: catalogSummaryLabel(progress),
        detail: catalogDetail(progress, progress.catalog.active_provider),
        tone,
      };
    case "body":
      return {
        label: progress.body.pending_jobs > 0
          ? `Loading details · ${progress.body.pending_jobs} remaining`
          : "Loading details",
        detail: activeBodyProvider
          ? `Loading ${activeBodyProvider} session details.`
          : "Loading saved session details.",
        tone,
      };
    case "waiting_to_retry":
      return {
        label: failures ? "Session index needs attention · retry scheduled" : "Session index refresh scheduled",
        detail: failures
          ? "A provider needs another attempt before indexing can continue."
          : "The scheduler will run another index pass shortly.",
        tone: failures ? "warning" : "neutral",
      };
    case "idle":
      return failures
        ? {
          label: "Session index needs attention",
          detail: "One or more providers could not be indexed.",
          tone: "warning",
        }
        : {
          label: "Session index is up to date",
          detail: "All known provider catalogs are current.",
          tone: "neutral",
        };
  }
}

interface NotificationCenterProps {
  progress: SessionIndexProgress | null;
  is_loading: boolean;
  error: string | null;
  source_errors: SourceError[];
  is_retrying: boolean;
  is_open: boolean;
  on_close: () => void;
  on_retry: () => void;
  trigger_ref: RefObject<HTMLButtonElement | null>;
}

export function NotificationCenter({
  progress,
  is_loading,
  error,
  source_errors,
  is_retrying,
  is_open,
  on_close,
  on_retry,
  trigger_ref,
}: NotificationCenterProps) {
  const dialogRef = useFloatingPanel(is_open, on_close, trigger_ref);
  const summary = describeSessionIndexProgress(progress, is_loading, error, source_errors);
  const failedProviders = useMemo(() => {
    const providers = new Set<ViewerProvider>();
    for (const provider of progress?.catalog.error_providers ?? []) {
      providers.add(provider);
    }
    for (const provider of progress?.body.providers ?? []) {
      if (provider.failed_jobs > 0) {
        providers.add(provider.provider);
      }
    }
    for (const sourceError of source_errors) {
      providers.add(sourceError.provider);
    }
    return [...providers];
  }, [progress, source_errors]);
  const sourceErrorsByProvider = useMemo(() => {
    const errors = new Map<ViewerProvider, string>();
    for (const sourceError of source_errors) {
      errors.set(sourceError.provider, sourceError.message);
    }
    return errors;
  }, [source_errors]);
  const workerError = progress?.worker_error ?? null;
  const currentProviderStatuses = useMemo(
    () => providerStatuses(progress, source_errors),
    [progress, source_errors],
  );

  if (!is_open) {
    return null;
  }

  return (
    <div
      aria-labelledby="notification-center-title"
      aria-modal="false"
      className="notification-center"
      data-tone={summary.tone}
      id="notification-center"
      ref={dialogRef}
      role="dialog"
      tabIndex={-1}
    >
      <div className="notification-center__header">
        <div>
          <p className="eyebrow">OPERATIONAL STATUS</p>
          <h2 id="notification-center-title">Notifications</h2>
        </div>
        <button aria-label="Close notifications" className="notification-center__close" onClick={on_close} type="button">
          <span aria-hidden="true">×</span>
        </button>
      </div>

      <section aria-busy={progress?.is_refreshing ?? is_loading} className="notification-task">
        <div className="notification-task__title-row">
          <div>
            <h3>Session index</h3>
            <p>{summary.label}</p>
          </div>
          {summary.tone === "warning" ? <WarningIcon className="notification-task__warning" /> : null}
        </div>
        <p className="notification-task__detail">{summary.detail}</p>

        {progress ? (
          <dl className="notification-task__metrics">
            <div>
              <dt>Providers checked</dt>
              <dd>{progress.catalog.processed_providers} / {progress.catalog.total_providers} providers</dd>
            </div>
            <div>
              <dt>Details queued</dt>
              <dd>{progress.body.pending_jobs} pending</dd>
            </div>
            <div>
              <dt>This batch</dt>
              <dd>{progress.body.completed_in_run} indexed{progress.body.stale_in_run > 0 ? ` · ${progress.body.stale_in_run} outdated attempts` : ""}</dd>
            </div>
          </dl>
        ) : null}

        {progress ? (
          <div className="notification-task__providers">
            <h4>Providers</h4>
            <ul>
              {currentProviderStatuses.map((provider) => (
                <li data-tone={provider.tone} key={provider.provider}>
                  <span>{providerLabel(provider.provider)}</span>
                  <span aria-label={`${provider.display_label}: ${provider.detail}`}>{provider.display_label}</span>
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        {error || workerError || failedProviders.length ? (
          <div className="notification-task__failures" role="status">
            <h4>Needs attention</h4>
            <ul>
              {error ? (
                <li>
                  <strong>Session index control</strong>
                  <span>{error}</span>
                </li>
              ) : null}
              {workerError ? (
                <li>
                  <strong>Session index worker</strong>
                  <span>{workerErrorMessage(workerError)}</span>
                </li>
              ) : null}
              {failedProviders.map((provider) => (
                <li key={provider}>
                  <strong>{providerLabel(provider)}</strong>
                  <span>{sourceErrorsByProvider.get(provider) ?? "Retry indexing to try this provider again."}</span>
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        <div className="notification-task__actions">
          <button disabled={is_retrying} onClick={on_retry} type="button">
            {is_retrying ? "Retrying indexing…" : "Retry indexing"}
          </button>
        </div>
      </section>
    </div>
  );
}
