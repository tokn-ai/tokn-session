import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { ManagedSessionDialogResponse, ManagedSessionSnapshot, RegisteredProject, RegisterProjectRequest } from "../lib/types";
import { errorMessage } from "../lib/state";
import { CloseIcon } from "./Icons";
import { ActionNotificationHost } from "./ActionNotifications";
import "./ProjectWorkspace.css";

function LastTurnFailure({ message }: { message?: string | null }) {
  if (!message?.trim()) return null;
  return <div className="managed-session-failure workspace-error" role="alert" aria-label="Last turn failed">
    <strong>Last turn failed</strong><span>{message}</span>
  </div>;
}

function WorkspaceDialog({ title, open = true, on_close, children }: {
  title: string; open?: boolean; on_close: () => void; children: ReactNode;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const title_id = useId();
  useEffect(() => {
    if (!open) { dialog.current?.close(); return; }
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.current?.showModal();
    return () => {
      dialog.current?.close();
      if (opener?.isConnected) opener.focus({ preventScroll: true });
    };
  }, [open]);
  return createPortal(<dialog className="workspace-dialog" ref={dialog} aria-labelledby={title_id}
    onCancel={(event) => { event.preventDefault(); on_close(); }}>
    <header><h2 id={title_id}>{title}</h2><button aria-label={`Close ${title}`} className="icon-button" type="button" onClick={on_close}><CloseIcon /></button></header>
    {children}
    {open && <ActionNotificationHost />}
  </dialog>, document.body);
}

export function RegisterProjectDialog({ open, on_close, on_register, on_registered }: {
  open: boolean; on_close: () => void;
  on_register: (request: RegisterProjectRequest) => Promise<RegisteredProject>;
  on_registered: (project: RegisteredProject) => void;
}) {
  const [folder_path, setFolderPath] = useState("");
  const [name, setName] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const busy = useRef(false);
  const alive = useRef(true);
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
  return <WorkspaceDialog title="Add project" open={open} on_close={on_close}>
    <form onSubmit={(event) => {
      event.preventDefault();
      if (busy.current || !folder_path.trim()) return;
      busy.current = true; setSaving(true); setError(null);
      void on_register({ folder_path: folder_path.trim(), name: name.trim() || null }).then((project) => {
        if (!alive.current) return;
        setFolderPath(""); setName(""); on_registered(project);
      }).catch((failure) => { if (alive.current) setError(errorMessage(failure)); })
        .finally(() => { busy.current = false; if (alive.current) setSaving(false); });
    }}>
      <p>Register a folder on this machine. Its existing files stay in place.</p>
      <label>Folder path<input autoFocus autoComplete="off" placeholder="/path/to/project" required value={folder_path} disabled={saving} onChange={(event) => setFolderPath(event.target.value)} /></label>
      <label>Name <span className="workspace-optional">(optional)</span><input value={name} disabled={saving} onChange={(event) => setName(event.target.value)} /></label>
      {error ? <p className="workspace-error" role="alert">{error}</p> : null}
      <footer><button type="button" onClick={on_close}>Cancel</button><button type="submit" disabled={saving || !folder_path.trim()}>{saving ? "Adding…" : "Add project"}</button></footer>
    </form>
  </WorkspaceDialog>;
}

export function NewSessionDialog({ open, projects, preferred_group_key, snapshot, creating, uncertain = false, error, control_error, controlling, on_close, on_create, on_stop, on_open }: {
  open: boolean; projects: RegisteredProject[]; preferred_group_key?: string;
  snapshot: ManagedSessionSnapshot | null; creating: boolean; uncertain?: boolean; error: string | null; control_error: string | null; controlling: boolean;
  on_close: () => void; on_create: (project_id: string, prompt: string) => Promise<void>;
  on_stop: (snapshot: ManagedSessionSnapshot) => void; on_open: () => void;
}) {
  const [project_id, setProjectId] = useState("");
  const [prompt, setPrompt] = useState("");
  const was_open = useRef(false);
  useEffect(() => { if (snapshot?.session) setPrompt(""); }, [snapshot?.session?.session_key]);
  useEffect(() => {
    if (open && !was_open.current && !snapshot && !uncertain) {
      const preferred = projects.find((project) => project.available && project.group_key === preferred_group_key)
        ?? projects.find((project) => project.available);
      setProjectId(preferred?.project_id ?? "");
    }
    was_open.current = open;
  }, [open, preferred_group_key, projects, snapshot, uncertain]);
  const pending = creating || !!snapshot && snapshot.status !== "failed" && snapshot.status !== "stopped" && !snapshot.session;
  const selected = projects.find((project) => project.project_id === project_id);
  return <WorkspaceDialog title="New Pi session" open={open} on_close={on_close}>
    <form onSubmit={(event) => { event.preventDefault(); void on_create(project_id, prompt); }}>
      <p>Pi uses your existing model and authentication settings on this machine.</p>
      <label>Project folder<select required disabled={pending || uncertain} value={project_id} onChange={(event) => setProjectId(event.target.value)}>
        <option value="" disabled>Choose a folder</option>
        {projects.map((project) => <option key={project.project_id} value={project.project_id} disabled={!project.available}>{project.name} · {project.folder_path}{project.available ? "" : " (unavailable)"}</option>)}
      </select></label>
      {selected ? <p className="workspace-folder">{selected.folder_path}</p> : null}
      <label>First message<textarea autoFocus rows={6} required value={prompt} disabled={pending || uncertain} onChange={(event) => setPrompt(event.target.value)} placeholder="What would you like Pi to do?" /></label>
      {pending ? <p className="workspace-progress" role="status"><span className="inline-spinner" />{snapshot?.status === "stopping" ? "Stopping Pi…" : "Starting Pi… Waiting for its conversation to be saved."}</p> : null}
      {snapshot?.message ? <p className={snapshot.status === "failed" ? "workspace-error" : "workspace-notice"} role={snapshot.status === "failed" ? "alert" : "status"}>{snapshot.message}</p> : null}
      <LastTurnFailure message={snapshot?.last_turn_error} />
      {error || control_error ? <p className="workspace-error" role="alert">{error ?? control_error}</p> : null}
      <footer>
        <button type="button" onClick={on_close}>Close</button>
        {snapshot && !snapshot.session && !["failed", "stopped"].includes(snapshot.status) ? <button type="button" disabled={controlling || snapshot.status === "stopping"} onClick={() => on_stop(snapshot)}>Stop</button> : null}
        {snapshot?.session ? <button type="button" onClick={on_open}>Open conversation</button>
          : <button type="submit" disabled={pending || !uncertain && (!selected?.available || !prompt.trim())}>{uncertain ? "Check session start" : error || snapshot?.status === "failed" || snapshot?.status === "stopped" ? "Try again" : "Start Pi session"}</button>}
      </footer>
    </form>
  </WorkspaceDialog>;
}

export function ManagedSessionControls({ snapshot, busy, error, on_control }: {
  snapshot: ManagedSessionSnapshot | null; busy: boolean; error: string | null;
  on_control: (kind: "stop" | "resume", snapshot: ManagedSessionSnapshot) => void;
}) {
  if (!snapshot) return null;
  const active = ["starting", "running", "stopping"].includes(snapshot.status);
  const resumable = snapshot.status === "stopped" || snapshot.status === "failed";
  const has_turn_error = !!snapshot.last_turn_error?.trim();
  return <div className="managed-session-controls" aria-label="Pi session controls">
    <span role="status">Pi · {snapshot.status === "idle" && !has_turn_error ? "Ready" : snapshot.status.charAt(0).toUpperCase() + snapshot.status.slice(1)}</span>
    {active ? <button type="button" disabled={busy || snapshot.status === "stopping"} onClick={() => on_control("stop", snapshot)}>Stop</button> : null}
    {resumable ? <button type="button" disabled={busy} onClick={() => on_control("resume", snapshot)}>{busy ? "Resuming…" : "Resume Pi"}</button> : null}
    {error || snapshot.status === "failed" && snapshot.message ? <span className="workspace-error" role="alert">{error ?? snapshot.message}</span> : null}
    <LastTurnFailure message={snapshot.last_turn_error} />
  </div>;
}

export function ManagedPiDialog({ snapshot, busy, error, on_respond }: {
  snapshot: ManagedSessionSnapshot; busy: boolean; error: string | null;
  on_respond: (response: ManagedSessionDialogResponse) => void;
}) {
  const request = snapshot.pending_dialog!;
  const [value, setValue] = useState(request.initial_value ?? "");
  const supported = ["confirm", "select", "input", "editor"].includes(request.kind);
  const respond = (cancelled: boolean, confirmed?: boolean) => on_respond({
    request_id: snapshot.request_id, dialog_id: request.dialog_id, cancelled,
    ...(request.kind === "confirm" ? { confirmed: confirmed ?? false } : { value: cancelled ? null : value }),
  });
  return <WorkspaceDialog title={request.title || "Pi needs input"} on_close={() => { if (!busy) respond(true); }}>
    <form onSubmit={(event) => { event.preventDefault(); if (!busy && supported) respond(false, true); }}>
      {request.message ? <p className="workspace-dialog-message">{request.message}</p> : null}
      {request.kind === "select" ? <label>Choice<select autoFocus required value={value} disabled={busy} onChange={(event) => setValue(event.target.value)}>
        <option value="" disabled>Choose an option</option>{request.options.map((option, index) => <option key={index} value={option}>{option}</option>)}
      </select></label> : request.kind === "input" ? <label>Response<input autoFocus value={value} disabled={busy} onChange={(event) => setValue(event.target.value)} /></label>
        : request.kind === "editor" ? <label>Response<textarea autoFocus rows={8} value={value} disabled={busy} onChange={(event) => setValue(event.target.value)} /></label>
          : !supported ? <p>This Pi dialog type is not supported. Cancel it to continue safely.</p> : null}
      {error ? <p role="alert" className="workspace-error">{error}</p> : null}
      <footer><button type="button" disabled={busy} onClick={() => respond(true)}>{request.kind === "confirm" ? "Decline" : "Cancel"}</button>
        {supported ? <button type="submit" disabled={busy || request.kind === "select" && !request.options.includes(value)}>{busy ? "Sending…" : request.kind === "confirm" ? "Confirm" : "Submit"}</button> : null}</footer>
    </form>
  </WorkspaceDialog>;
}
