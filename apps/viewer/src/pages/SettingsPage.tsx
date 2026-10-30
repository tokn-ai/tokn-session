import { useEffect, useId, useState, type ReactNode } from "react";
import { BellIcon, ChevronIcon, CloseIcon, PanelIcon, TechnicalIcon } from "../components/Icons";
import { providerLabel } from "../lib/state";
import type { MachineSettingsSnapshot, RelaySettings, RelayStatus, ViewerPreferences } from "../lib/types";
import { RuntimeSettings } from "../components/RuntimeSettings";
import { RpcRuntimeSettings } from "../components/RpcRuntimeSettings";
import "./SettingsPage.css";

export interface SettingsPageProps {
  initial_section?: SettingsSection;
  on_close: () => void;
  machine_label: string;
  machine_key: string;
  machine_connected?: boolean;
  preferences: ViewerPreferences;
  on_preferences_change: (partial: Partial<ViewerPreferences>) => void;
  on_preferences_reset: () => void;
  settings: MachineSettingsSnapshot | null;
  settings_loading: boolean;
  settings_error: string | null;
  on_settings_retry: () => void;
  on_reader_apply: (draft: RelaySettings) => Promise<void>;
  hosting: ReactNode;
}

export type SettingsSection = "viewer" | "sources" | "hosting" | "runtimes" | "agent_runtimes";

const READER_PHASES: Record<RelayStatus["phase"], string> = {
  local: "Local history", starting: "Starting", connecting: "Connecting", live: "Live",
  reconnecting: "Reconnecting", retrying: "Retrying", failed: "Needs attention",
};

function SelectField({ id, label, description, value, on_change, children }: {
  id: string;
  label: string;
  description: string;
  value: string;
  on_change: (value: string) => void;
  children: ReactNode;
}) {
  return <div className="settings-page__row">
    <div><label htmlFor={id}>{label}</label><p id={`${id}-description`}>{description}</p></div>
    <div className="settings-page__select">
      <select aria-describedby={`${id}-description`} id={id} value={value} onChange={(event) => on_change(event.currentTarget.value)}>{children}</select>
      <ChevronIcon />
    </div>
  </div>;
}

function ViewerPreferencesSection({ preferences, on_change, on_reset }: {
  preferences: ViewerPreferences;
  on_change: SettingsPageProps["on_preferences_change"];
  on_reset: () => void;
}) {
  const id = useId();
  return <>
    <div className="settings-page__card">
      <h3>Appearance</h3>
      <SelectField id={`${id}-order`} label="Sidebar grouping" description="Organize sessions by recent activity or project." value={preferences.sidebar_order} on_change={(sidebar_order) => on_change({ sidebar_order: sidebar_order as ViewerPreferences["sidebar_order"] })}>
        <option value="time">Time</option><option value="project">Projects</option>
      </SelectField>
      <div className="settings-page__row">
        <div><label htmlFor={`${id}-lifecycle`}>Lifecycle events</label><p id={`${id}-lifecycle-description`}>Show session bookkeeping inside activity groups.</p></div>
        <input aria-describedby={`${id}-lifecycle-description`} id={`${id}-lifecycle`} type="checkbox" checked={preferences.show_lifecycle} onChange={(event) => on_change({ show_lifecycle: event.currentTarget.checked })} />
      </div>
    </div>
    <div className="settings-page__card">
      <h3>Live updates</h3>
      <SelectField id={`${id}-scope`} label="Background sessions" description="Choose which sessions keep their sidebar previews and status up to date." value={preferences.background_updates_scope} on_change={(background_updates_scope) => on_change({ background_updates_scope: background_updates_scope as ViewerPreferences["background_updates_scope"] })}>
        <option value="all">All sessions</option><option value="sidebar">Sidebar sessions</option>
      </SelectField>
      <SelectField id={`${id}-background`} label="Background delivery" description={preferences.background_update_level === "final" ? "Final assistant replies and attention updates." : "Intermediate and final assistant messages, plus attention updates."} value={preferences.background_update_level} on_change={(background_update_level) => on_change({ background_update_level: background_update_level as ViewerPreferences["background_update_level"] })}>
        <option value="final">Final replies</option><option value="messages">All assistant messages</option>
      </SelectField>
      <SelectField id={`${id}-conversation`} label="Conversation delivery" description="The level used for open conversations. Expand groups to load details on demand." value={preferences.conversation_update_level} on_change={(conversation_update_level) => on_change({ conversation_update_level: conversation_update_level as ViewerPreferences["conversation_update_level"] })}>
        <option value="final">Final replies</option><option value="messages">Messages</option><option value="steps">Steps</option><option value="all">All events</option>
      </SelectField>
    </div>
    <div className="settings-page__reset"><p>Preferences are saved on this device and apply immediately.</p><button type="button" onClick={on_reset}>Reset viewer preferences</button></div>
  </>;
}

function sameReaderSettings(left: RelaySettings, right: RelaySettings) {
  return left.mode === right.mode && left.endpoint === right.endpoint && left.include_native === right.include_native;
}

function ReaderSettings({ reader, on_apply }: {
  reader: MachineSettingsSnapshot["reader"];
  on_apply: SettingsPageProps["on_reader_apply"];
}) {
  const id = useId();
  const [draft, setDraft] = useState<RelaySettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const settings = draft ?? reader.status.settings;
  const dirty = !sameReaderSettings(settings, reader.status.settings);
  const disabled = busy || !reader.editable;
  useEffect(() => {
    if (draft && sameReaderSettings(draft, reader.status.settings)) setDraft(null);
  }, [draft, reader.status.settings]);
  function change(partial: Partial<RelaySettings>) {
    setDraft({ ...settings, ...partial });
    setSaved(false);
    setError(null);
  }
  async function apply() {
    if (disabled || !dirty || (settings.mode === "external" && !settings.endpoint.trim())) return;
    setBusy(true); setError(null); setSaved(false);
    try {
      await on_apply({ ...settings, endpoint: settings.endpoint.trim() });
      setDraft(null); setSaved(true);
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    } finally { setBusy(false); }
  }
  return <div className="settings-page__card">
    <div className="settings-page__card-heading"><h3>Session reader</h3><span className="settings-page__status" data-phase={reader.status.phase}><span aria-hidden="true" />{READER_PHASES[reader.status.phase]}</span></div>
    {!reader.editable && <p className="settings-page__notice">{reader.edit_reason ?? "Reader settings are read-only on this machine."}</p>}
    <form onSubmit={(event) => { event.preventDefault(); void apply(); }}>
      <div className="settings-page__row">
        <div><label htmlFor={`${id}-mode`}>Reader mode</label><p id={`${id}-description`}>{settings.mode === "automatic" ? "Read saved sessions and receive live updates automatically." : settings.mode === "external" ? "Connect to a separately managed Relay service on this machine." : "Read saved sessions without a live Relay connection."}</p></div>
        <div className="settings-page__select"><select aria-describedby={`${id}-description`} id={`${id}-mode`} value={settings.mode} disabled={disabled} onChange={(event) => change({ mode: event.currentTarget.value as RelaySettings["mode"] })}>
          <option value="automatic">Automatic (recommended)</option><option value="external">External Relay</option><option value="local">Local history only</option>
        </select><ChevronIcon /></div>
      </div>
      {settings.mode === "external" && <div className="settings-page__row settings-page__row--stacked"><div><label htmlFor={`${id}-endpoint`}>Relay endpoint</label><p id={`${id}-endpoint-description`}>The address of the Relay snapshot service.</p></div>
        <input aria-describedby={`${id}-endpoint-description`} id={`${id}-endpoint`} required spellCheck={false} type="text" value={settings.endpoint} disabled={disabled} onChange={(event) => change({ endpoint: event.currentTarget.value })} /></div>}
      {settings.mode === "automatic" && <div className="settings-page__row"><div><label htmlFor={`${id}-native`}>Native inspection records</label><p id={`${id}-native-description`}>Keep provider-specific records available in the inspector.</p></div>
        <input aria-describedby={`${id}-native-description`} id={`${id}-native`} type="checkbox" checked={settings.include_native} disabled={disabled} onChange={(event) => change({ include_native: event.currentTarget.checked })} /></div>}
      {reader.status.settings.mode === "external" && reader.status.active_endpoint && <p className="settings-page__hint">Active endpoint: <code>{reader.status.active_endpoint}</code></p>}
      {reader.status.error && <p className="settings-page__error" role="alert">{reader.status.error}</p>}
      {error && <p className="settings-page__error" role="alert">{error}</p>}
      {reader.editable && <div className="settings-page__actions"><span aria-live="polite">{busy ? "Applying reader settings…" : saved ? "Reader settings applied." : dirty ? "Unsaved changes" : ""}</span>
        {dirty && <button type="button" disabled={busy} onClick={() => { setDraft(null); setError(null); setSaved(false); }}>Discard changes</button>}
        <button className="settings-page__primary" type="submit" disabled={busy || !dirty || (settings.mode === "external" && !settings.endpoint.trim())}>{busy ? "Applying…" : "Apply reader settings"}</button>
      </div>}
    </form>
  </div>;
}

function SourceLocations({ settings }: { settings: MachineSettingsSnapshot }) {
  return <div className="settings-page__card">
    <div className="settings-page__card-heading"><h3>Session sources</h3><span className="settings-page__scope">Detected locations</span></div>
    <p className="settings-page__hint">{settings.sources_edit_reason ?? "These locations are detected on the selected machine."}</p>
    {settings.reader.status.settings.mode === "external" && <p className="settings-page__hint">The external Relay may use different source locations.</p>}
    <div className="settings-page__sources">
      {settings.sources.map((source) => <div className="settings-page__source" key={source.provider}>
        <strong><span aria-hidden="true" className="settings-page__provider" data-provider={source.provider} />{providerLabel(source.provider)}</strong>
        <div>{source.locations.length === 0 && !source.error && <p>No source location reported.</p>}
          {source.locations.map((location) => <div className="settings-page__location" key={`${location.kind}:${location.path}`}><code>{location.path}</code><span>{location.exists ? location.kind === "database" ? "Database" : "Directory" : "Not found"}</span></div>)}
          {source.error && <p className="settings-page__error" role="alert">{source.error}</p>}
        </div>
      </div>)}
    </div>
  </div>;
}

export function SettingsPage({ on_close, initial_section = "viewer", machine_label, machine_key, machine_connected = true, preferences, on_preferences_change, on_preferences_reset, settings, settings_loading, settings_error, on_settings_retry, on_reader_apply, hosting }: SettingsPageProps) {
  const id = useId();
  const [section, setSection] = useState<SettingsSection>(initial_section);
  const sections: { key: SettingsSection; label: string; detail: string; icon: ReactNode }[] = [
    { key: "viewer", label: "Viewer preferences", detail: "This device", icon: <PanelIcon /> },
    { key: "sources", label: "Sources & reader", detail: machine_label, icon: <TechnicalIcon /> },
    { key: "hosting", label: "Hosting & access", detail: machine_label, icon: <BellIcon /> },
    { key: "agent_runtimes", label: "Agent runtimes", detail: machine_label, icon: <TechnicalIcon /> },
    { key: "runtimes", label: "Runtimes", detail: "This viewer", icon: <TechnicalIcon /> },
  ];
  return <div className="settings-page">
    <header className="settings-page__header"><h1>Settings</h1><button aria-label="Close settings" className="icon-button" onClick={on_close} type="button"><CloseIcon /></button></header>
    <div className="settings-page__layout">
      <nav aria-label="Settings sections" className="settings-page__nav">
        {sections.map((item) => <button aria-controls={`${id}-${item.key}`} aria-current={section === item.key ? "page" : undefined} className="settings-page__nav-item" key={item.key} onClick={() => setSection(item.key)} type="button">{item.icon}<span><strong>{item.label}</strong><small>{item.detail}</small></span></button>)}
      </nav>
      <div className="settings-page__content">
        <section aria-labelledby={`${id}-viewer-title`} className="settings-page__section" hidden={section !== "viewer"} id={`${id}-viewer`}>
          <div className="settings-page__section-heading"><span className="settings-page__scope">This device</span><h2 id={`${id}-viewer-title`}>Viewer preferences</h2><p>Choose how sessions appear and stay up to date.</p></div>
          <ViewerPreferencesSection preferences={preferences} on_change={on_preferences_change} on_reset={on_preferences_reset} />
        </section>
        <section aria-labelledby={`${id}-sources-title`} className="settings-page__section" hidden={section !== "sources"} id={`${id}-sources`}>
          <div className="settings-page__section-heading"><span className="settings-page__scope">{machine_label}</span><h2 id={`${id}-sources-title`}>Sources & reader</h2><p>Session discovery and reading on the selected machine.</p></div>
          {settings_error && <div className="settings-page__empty"><p className="settings-page__error" role="alert">{settings_error}</p><button onClick={on_settings_retry} type="button">Retry machine settings</button></div>}
          {!settings ? !settings_error && <div className="settings-page__empty"><p role="status">{!machine_connected ? "Choose a machine to view its session sources and reader settings." : settings_loading ? "Loading machine settings…" : "Machine settings are unavailable."}</p>{machine_connected && !settings_loading && <button onClick={on_settings_retry} type="button">Retry machine settings</button>}</div>
            : <><ReaderSettings key={machine_key} reader={settings.reader} on_apply={on_reader_apply} /><SourceLocations settings={settings} /></>}
        </section>
        <section aria-labelledby={`${id}-hosting-title`} className="settings-page__section" hidden={section !== "hosting"} id={`${id}-hosting`}>
          <div className="settings-page__section-heading"><span className="settings-page__scope">{machine_label}</span><h2 id={`${id}-hosting-title`}>Hosting & access</h2><p>Availability and access for the selected machine.</p></div>
          {hosting ?? <div className="settings-page__card"><p className="settings-page__notice">{settings?.hosting.reason ?? "Hosting settings are unavailable on this machine."}</p></div>}
        </section>
        <section aria-labelledby={`${id}-runtimes-title`} className="settings-page__section" hidden={section !== "runtimes"} id={`${id}-runtimes`}>
          <RuntimeSettings active={section === "runtimes"} heading_id={`${id}-runtimes-title`} />
        </section>
        <section aria-labelledby={`${id}-agent-runtimes-title`} className="settings-page__section" hidden={section !== "agent_runtimes"} id={`${id}-agent_runtimes`}>
          <RpcRuntimeSettings key={machine_key} active={section === "agent_runtimes"} connected={machine_connected} machine_key={machine_key} machine_label={machine_label} heading_id={`${id}-agent-runtimes-title`} />
        </section>
      </div>
    </div>
  </div>;
}
