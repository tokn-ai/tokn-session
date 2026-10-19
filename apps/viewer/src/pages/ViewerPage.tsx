import { useRef, useState, type ReactNode } from "react";
import { Conversation } from "../components/Conversation";
import { RelayConnection } from "../components/RelayConnection";
import { Inspector } from "../components/Inspector";
import { SessionDrawer } from "../components/SessionDrawer";
import { Sidebar } from "../components/Sidebar";
import { StatusBar } from "../components/StatusBar";
import { QuestionAnswersProvider } from "../components/QuestionAnswersProvider";
import { TranslationProvider } from "../components/TranslationProvider";
import { useViewerState } from "../lib/useViewerState";
import { SettingsDialog } from "../components/SettingsDialog";
import { ManagedPiDialog, ManagedSessionControls, NewSessionDialog, RegisterProjectDialog } from "../components/ProjectWorkspace";
import { useProjectWorkspace } from "../lib/useProjectWorkspace";
import { isDesktop } from "../lib/transport";

interface ViewerPageProps {
  remote?: boolean;
  connection?: ReactNode;
  on_open_machines?: () => void;
  machine_label?: string;
  machine_key?: string;
}

export function ViewerPage({ remote = false, connection, on_open_machines, machine_label, machine_key }: ViewerPageProps) {
  return <TranslationProvider><QuestionAnswersProvider><ViewerContent remote={remote} connection={connection} on_open_machines={on_open_machines}
    machine_label={machine_label} machine_key={machine_key} /></QuestionAnswersProvider></TranslationProvider>;
}

function ViewerContent({ remote = false, connection, on_open_machines, machine_label = remote ? "Selected machine" : "This computer", machine_key = remote ? "remote" : "local" }: ViewerPageProps) {
  const viewer = useViewerState();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [projectOpen, setProjectOpen] = useState(false);
  const [newSessionOpen, setNewSessionOpen] = useState(false);
  const [preferredGroupKey, setPreferredGroupKey] = useState<string | undefined>();
  const localWorkspace = !remote && isDesktop();
  const workspace = useProjectWorkspace({ enabled: localWorkspace, machine_key: machine_key!,
    selected_session_key: viewer.selectedSessionKey,
    on_created: (session) => { viewer.openCreatedSession(session); setNewSessionOpen(false); },
  });
  const pendingDialog = workspace.pending_dialog_session;
  const settingsTrigger = useRef<HTMLButtonElement>(null);

  function newSession(group_key?: string) {
    workspace.beginCreation();
    setPreferredGroupKey(group_key);
    setNewSessionOpen(true);
  }

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
            registered_projects={workspace.workspace?.projects}
            workspace_available={localWorkspace && workspace.workspace?.available === true}
            workspace_notice={workspace.error ?? workspace.workspace?.unavailable_reason}
            on_workspace_retry={workspace.error ? workspace.retry : undefined}
            on_add_project={localWorkspace ? () => setProjectOpen(true) : undefined}
            on_new_session={localWorkspace ? newSession : undefined}
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
          input_refresh_key={workspace.input_refresh_key}
          managed_controls={<ManagedSessionControls snapshot={workspace.managed} busy={workspace.controlling}
            error={workspace.control_error} on_control={(kind, snapshot) => { void workspace.control(kind, snapshot); }} />}
          outstanding_questions={viewer.outstandingQuestions}
          question_navigation={viewer.questionNavigation}
          on_question_open={viewer.openQuestion}
          pending_live_activity={viewer.pendingLiveActivity}
          on_show_live_activity={viewer.showLiveActivity}
          on_follow_change={viewer.setFollowingLive}
          on_input_accepted={viewer.refreshSessionAfterInput}
          error={viewer.eventsError}
          follow_error={viewer.followError}
          events={viewer.events}
          expanded_detail={viewer.expandedDetail}
          expanded_detail_error={viewer.expandedDetailError}
          expanded_detail_loading={viewer.expandedDetailLoading}
          expanded_event_key={viewer.expandedEventKey}
          expanded_activity_keys={viewer.expandedActivityKeys}
          expanded_activities={viewer.expandedActivities}
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
          on_trajectory_retry={viewer.retryTrajectoryEvents}
          on_activity_load={viewer.loadActivityGroup}
          on_activity_visibility={viewer.setActivityGroupVisibility}
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
        on_open_settings={() => setSettingsOpen(true)}
        settings_trigger_ref={settingsTrigger}
        background_updates_scope={viewer.backgroundUpdatesScope}
        background_updates_level={viewer.backgroundUpdatesLevel}
        background_updates_status={viewer.backgroundUpdatesStatus}
        conversation_update_level={viewer.conversationUpdateLevel}
        conversation_available={viewer.selectedSessionKey !== null}
        on_background_updates_scope_change={viewer.setBackgroundUpdatesScope}
        on_background_updates_level_change={viewer.setBackgroundUpdatesLevel}
        on_conversation_update_level_change={viewer.setConversationUpdateLevel}
        connection={remote ? connection : <RelayConnection on_open_machines={on_open_machines} />}
        error={viewer.sessionIndexProgressError}
        is_loading={viewer.sessionIndexProgressLoading}
        is_retrying={viewer.sessionIndexRetrying}
        on_retry={viewer.retrySessionIndex}
        progress={viewer.sessionIndexProgress}
        source_errors={viewer.sourceErrors}
      />
      {settingsOpen && <SettingsDialog key={machine_key} remote={remote} machine_label={machine_label}
        machine_key={machine_key} return_focus={settingsTrigger} on_close={() => setSettingsOpen(false)} />}
      {localWorkspace ? <>
        <RegisterProjectDialog key={`project:${machine_key}`} open={projectOpen} on_close={() => setProjectOpen(false)}
          on_register={workspace.addProject} on_registered={() => { setProjectOpen(false); viewer.setSessionOrder("project"); }} />
        <NewSessionDialog key={`session:${machine_key}`} open={newSessionOpen} projects={workspace.workspace?.projects ?? []}
          preferred_group_key={preferredGroupKey} snapshot={workspace.creation} creating={workspace.creating}
          uncertain={workspace.creation_uncertain}
          error={workspace.creation_error} control_error={workspace.creation_control_error} controlling={workspace.controlling}
          on_create={workspace.createSession} on_stop={(snapshot) => { void workspace.control("stop", snapshot); }}
          on_open={workspace.openCreation} on_close={() => { workspace.dismissCreation(); setNewSessionOpen(false); }} />
        {!newSessionOpen && workspace.creation && !workspace.creation.session && !["failed", "stopped"].includes(workspace.creation.status) ?
          <button className="workspace-pending-session" type="button" onClick={() => setNewSessionOpen(true)}>Pi session starting · View</button> : null}
        {pendingDialog ? <ManagedPiDialog key={`${pendingDialog.request_id}:${pendingDialog.pending_dialog!.dialog_id}`}
          snapshot={pendingDialog} busy={workspace.controlling} error={workspace.dialog_error}
          on_respond={(response) => { void workspace.respond(response); }} /> : null}
      </> : null}
    </div>
  );
}
