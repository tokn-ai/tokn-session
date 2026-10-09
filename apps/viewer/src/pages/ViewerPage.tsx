import { useState, type ReactNode } from "react";
import { Conversation } from "../components/Conversation";
import { RelayConnection } from "../components/RelayConnection";
import { Inspector } from "../components/Inspector";
import { SessionDrawer } from "../components/SessionDrawer";
import { Sidebar } from "../components/Sidebar";
import { StatusBar } from "../components/StatusBar";
import { TranslationProvider } from "../components/TranslationProvider";
import { useViewerState } from "../lib/useViewerState";

interface ViewerPageProps {
  remote?: boolean;
  connection?: ReactNode;
}

export function ViewerPage({ remote = false, connection }: ViewerPageProps) {
  return <TranslationProvider><ViewerContent remote={remote} connection={connection} /></TranslationProvider>;
}

function ViewerContent({ remote, connection }: ViewerPageProps) {
  const viewer = useViewerState();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);

  function openSessions() {
    if (window.matchMedia("(max-width: 860px)").matches) {
      viewer.setMobileSidebarOpen(true);
    } else {
      setSidebarCollapsed(false);
      window.requestAnimationFrame(() => document.querySelector<HTMLInputElement>('.sidebar input[type="search"]')?.focus());
    }
  }

  function collapseSidebar() {
    setSidebarCollapsed(true);
    window.requestAnimationFrame(() => document.querySelector<HTMLButtonElement>('.conversation button[aria-label="Open sessions"]')?.focus());
  }

  return (
    <div className="viewer-app" data-remote={remote}>
      <div className="viewer-shell" data-inspector-open={viewer.inspectorOpen} data-sidebar-collapsed={sidebarCollapsed}>
        <SessionDrawer desktop_hidden={sidebarCollapsed} is_open={viewer.mobileSidebarOpen} on_close={() => viewer.setMobileSidebarOpen(false)}>
          <Sidebar
            order={viewer.sessionOrder}
            on_order_change={viewer.setSessionOrder}
            on_close={() => viewer.setMobileSidebarOpen(false)}
            on_collapse={collapseSidebar}
            enabled_providers={viewer.enabledProviders}
            error={viewer.sessionsError}
            has_more={viewer.sessionsCursor !== null}
            is_loading={viewer.sessionsLoading}
            is_loading_more={viewer.sessionsLoadingMore}
            on_children_load={viewer.loadSessionChildren}
            on_children_load_more={viewer.loadMoreSessionChildren}
            on_children_retry={viewer.retrySessionChildren}
            on_load_more={viewer.loadMoreSessions}
            on_provider_toggle={viewer.toggleProvider}
            on_retry={viewer.retrySessions}
            on_search_change={viewer.setSearch}
            on_session_select={viewer.selectSession}
            on_question_session_select={viewer.selectQuestionSession}
            pending_providers={viewer.pendingProviders}
            search={viewer.search}
            session_children={viewer.sessionChildren}
            selected_session_key={viewer.selectedSessionKey}
            sessions={viewer.sessions}
            source_errors={viewer.sourceErrors}
          />
        </SessionDrawer>

        <Conversation
          outstanding_questions={viewer.outstandingQuestions}
          question_navigation={viewer.questionNavigation}
          on_question_open={viewer.openQuestion}
          pending_live_activity={viewer.pendingLiveActivity}
          on_show_live_activity={viewer.showLiveActivity}
          on_follow_change={viewer.setFollowingLive}
          on_input_accepted={viewer.refreshSessionAfterInput}
          error={viewer.eventsError}
          events={viewer.events}
          expanded_detail={viewer.expandedDetail}
          expanded_detail_error={viewer.expandedDetailError}
          expanded_detail_loading={viewer.expandedDetailLoading}
          expanded_event_key={viewer.expandedEventKey}
          has_newer={viewer.newerCursor !== null}
          has_older={viewer.olderCursor !== null}
          history_status={viewer.historyStatus}
          initial_page_loaded={viewer.initialPageLoaded}
          inspector_open={viewer.inspectorOpen}
          is_loading={viewer.eventsLoading}
          is_loading_newer={viewer.newerLoading}
          is_loading_older={viewer.olderLoading}
          on_event_select={viewer.selectEvent}
          on_event_toggle={viewer.toggleEventExpanded}
          on_open_related_session={viewer.openRelatedSession}
          on_inspector_toggle={viewer.toggleInspector}
          on_load_newer={viewer.loadNewerEvents}
          on_load_older={viewer.loadOlderEvents}
          on_retry={viewer.retryEvents}
          on_retry_expanded_detail={viewer.retryExpandedDetail}
          on_sidebar_open={openSessions}
          on_trajectory_load_newer={viewer.loadNewerTrajectoryEvents}
          on_trajectory_load_older={viewer.loadOlderTrajectoryEvents}
          on_trajectory_retry={viewer.retryTrajectoryEvents}
          on_trajectory_event_toggle={viewer.toggleTrajectoryEventExpanded}
          on_trajectory_retry_expanded_detail={viewer.retryExpandedTrajectoryDetail}
          selected_event_key={viewer.selectedEventKey}
          session={viewer.selectedSession}
          total_events={viewer.totalEvents}
          trajectory_expanded_detail={viewer.expandedTrajectoryDetail}
          trajectory_expanded_detail_error={viewer.expandedTrajectoryDetailError}
          trajectory_expanded_detail_loading={viewer.expandedTrajectoryDetailLoading}
          trajectory_expanded_event_key={viewer.expandedTrajectoryEventKey}
          trajectory_expanded_key={viewer.expandedTrajectoryKey}
          trajectory_pages={viewer.trajectoryPages}
        />

        <Inspector
          detail={viewer.detail}
          error={viewer.detailError}
          event={viewer.selectedEvent}
          is_loading={viewer.detailLoading}
          is_open={viewer.inspectorOpen}
          on_close={viewer.closeInspector}
          on_retry={viewer.retryDetail}
        />
      </div>

      <StatusBar
        connection={remote ? connection : <RelayConnection />}
        error={viewer.sessionIndexProgressError}
        is_loading={viewer.sessionIndexProgressLoading}
        is_retrying={viewer.sessionIndexRetrying}
        on_retry={viewer.retrySessionIndex}
        progress={viewer.sessionIndexProgress}
        source_errors={viewer.sourceErrors}
      />
    </div>
  );
}
