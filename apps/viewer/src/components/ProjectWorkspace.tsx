import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { ManagedSessionDialogResponse, ManagedSessionSnapshot, RegisteredProject, RegisterProjectRequest, RpcProvider, SessionCreationOptions, SessionCreationSelection } from "../lib/types";
import { errorMessage } from "../lib/state";
import { CloseIcon } from "./Icons";
import { ActionNotificationHost } from "./ActionNotifications";
import "./ProjectWorkspace.css";
import { ImageAttachmentPreviews, ImageAttachmentPicker, imageDrop, imagePaste } from "./ImageAttachments";
import { addImageDrafts, disposeImageDrafts, DEFAULT_IMAGE_LIMITS, type ImageDraft } from "../lib/imageDrafts";

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

const AGENT_NAMES: Record<RpcProvider, string> = { pi: "Pi", codex: "Codex", open_code: "OpenCode", dsh: "DSH" };

export function NewSessionDialog({ open, projects, preferred_group_key, snapshot, creating, uncertain = false, error, control_error, controlling, on_close, on_create, on_stop, on_open, on_load_options }: {
  open: boolean; projects: RegisteredProject[]; preferred_group_key?: string;
  snapshot: ManagedSessionSnapshot | null; creating: boolean; uncertain?: boolean; error: string | null; control_error: string | null; controlling: boolean;
  on_close: () => void; on_create: (project_id: string, prompt: string, images?: ImageDraft[], selection?: SessionCreationSelection) => Promise<void>;
  on_load_options: (project_id: string) => Promise<SessionCreationOptions>;
  on_stop: (snapshot: ManagedSessionSnapshot) => void; on_open: () => void;
}) {
  const [project_id, setProjectId] = useState("");
  const submitted_selection = useRef<SessionCreationSelection | null>(null);
  const [prompt, setPrompt] = useState("");
  const [provider, setProvider] = useState<RpcProvider>("pi");
  const [runtime_profile_id, setRuntimeProfileId] = useState("");
  const [options, setOptions] = useState<SessionCreationOptions | null>(null);
  const [options_loading, setOptionsLoading] = useState(false);
  const [options_error, setOptionsError] = useState<string | null>(null);
  const [options_retry, setOptionsRetry] = useState(0);
  useEffect(() => {
    if (!open || !project_id) return;
    let cancelled = false;
    setOptions(null); setOptionsLoading(true); setOptionsError(null);
    void on_load_options(project_id).then((result) => {
      if (!cancelled) setOptions(result);
    }).catch((failure) => { if (!cancelled) setOptionsError(errorMessage(failure)); })
      .finally(() => { if (!cancelled) setOptionsLoading(false); });
    return () => { cancelled = true; };
  }, [open, project_id, on_load_options, options_retry]);
  useEffect(() => { setRuntimeProfileId(""); }, [project_id, provider]);
  const [images, setImages] = useState<ImageDraft[]>([]);
  const [image_error, setImageError] = useState<string | null>(null);
  const image_ref = useRef(images);
  image_ref.current = images;
  useEffect(() => () => disposeImageDrafts(image_ref.current), []);
  const was_open = useRef(false);
  useEffect(() => { if (snapshot?.session && (snapshot.provider === "pi" || snapshot.creation_input_state === "accepted")) {
    setPrompt(""); disposeImageDrafts(image_ref.current); setImages([]); setImageError(null);
  } }, [snapshot?.session?.session_key, snapshot?.provider, snapshot?.creation_input_state]);
  useEffect(() => {
    if (open && !was_open.current && !snapshot && !uncertain) {
      const preferred = projects.find((project) => project.available && project.group_key === preferred_group_key)
        ?? projects.find((project) => project.available);
      setProjectId(preferred?.project_id ?? "");
    }
    was_open.current = open;
  }, [open, preferred_group_key, projects, snapshot, uncertain]);
  const rpc_receipt = snapshot?.provider !== "pi" && !!snapshot?.creation_phase && !snapshot.session;
  const checking_existing = uncertain || rpc_receipt;
  const rpc_settled = rpc_receipt && (["native_create_uncertain", "first_input_uncertain", "failed", "history_pending"].includes(snapshot.creation_phase!) || snapshot.creation_input_state === "rejected");
  const pending = creating || !!snapshot && !rpc_settled && snapshot.status !== "failed" && snapshot.status !== "stopped" && !snapshot.session;
  const selected = projects.find((project) => project.project_id === project_id);
  const agent = options?.project_id === project_id ? options.providers.find((option) => option.provider === provider) : undefined;
  const runtime = agent?.runtimes.find((entry) => entry.profile_id === runtime_profile_id);
  const agent_name = AGENT_NAMES[provider];
  const can_create = !!agent && (runtime_profile_id ? runtime?.available === true : agent.available);
  const selection: SessionCreationSelection = { provider, ...(runtime ? { runtime_profile_id: runtime.profile_id, runtime_revision: runtime.revision } : {}) };
  const images_allowed = agent?.supports_images === true;
  const addImages = (files: File[]) => {
    if (pending || checking_existing || !images_allowed) return;
    const result = addImageDrafts(image_ref.current, files, DEFAULT_IMAGE_LIMITS);
    setImages(result.images); setImageError(result.error);
  };
  return <WorkspaceDialog title="New session" open={open} on_close={on_close}>
    <form onSubmit={(event) => { event.preventDefault();
      if (!pending && !checking_existing && (!can_create || images.length && !images_allowed)) return;
      const chosen = checking_existing ? submitted_selection.current ?? selection : selection;
      if (!checking_existing) submitted_selection.current = { ...selection };
      void on_create(project_id, prompt, images, chosen);
    }} onPaste={(event) => { if (!pending && !checking_existing) imagePaste(event, addImages); }}
      onDragOver={(event) => { if (!pending && !checking_existing && event.dataTransfer.types.includes("Files")) event.preventDefault(); }}
      onDrop={(event) => { if (!pending && !checking_existing) imageDrop(event, addImages); }}>
      <p>Use an agent’s existing model and authentication settings on this machine.</p>
      <label>Agent<select value={provider} disabled={pending || checking_existing} onChange={(event) => setProvider(event.target.value as RpcProvider)}>
        {(Object.keys(AGENT_NAMES) as RpcProvider[]).map((value) => <option key={value} value={value}>{AGENT_NAMES[value]}</option>)}
      </select></label>
      <label>Project folder<select required disabled={pending || checking_existing} value={project_id} onChange={(event) => setProjectId(event.target.value)}>
        <option value="" disabled>Choose a folder</option>
        {projects.map((project) => <option key={project.project_id} value={project.project_id} disabled={!project.available}>{project.name} · {project.folder_path}{project.available ? "" : " (unavailable)"}</option>)}
      </select></label>
      {selected ? <p className="workspace-folder">{selected.folder_path}</p> : null}
      {options_loading ? <p role="status">Checking agent configuration…</p> : null}
      {options_error ? <p className="workspace-error" role="alert">{options_error} <button type="button" onClick={() => setOptionsRetry((value) => value + 1)}>Retry</button></p> : null}
      {!!agent?.runtimes.length && <label>Runtime<select value={runtime_profile_id} disabled={pending || checking_existing} onChange={(event) => setRuntimeProfileId(event.target.value)}>
        <option value="">Automatic{agent.default_profile_id ? ` · ${agent.runtimes.find((entry) => entry.profile_id === agent.default_profile_id)?.name ?? "Default"}` : ""}</option>
        {agent.runtimes.map((entry) => <option key={entry.profile_id} value={entry.profile_id} disabled={!entry.available}>{entry.name}{entry.available ? "" : " (unavailable)"}</option>)}
      </select></label>}
      {runtime?.reason || !runtime_profile_id && agent?.reason ? <p className={can_create ? "workspace-notice" : "workspace-error"} role={can_create ? "status" : "alert"}>{runtime?.reason ?? agent?.reason}</p> : null}
      {!!images.length && !images_allowed && <p className="workspace-error" role="alert">{agent_name} cannot accept these images. Remove them or choose an agent with image support.</p>}
      <ImageAttachmentPreviews images={images} disabled={pending || checking_existing} on_remove={(draft_id) => {
        if (pending || checking_existing) return;
        disposeImageDrafts(images.filter((image) => image.draft_id === draft_id));
        setImages(images.filter((image) => image.draft_id !== draft_id)); setImageError(null);
      }} />
      <label>First message<textarea autoFocus rows={6} required={!images.length} value={prompt} disabled={pending || checking_existing} onChange={(event) => setPrompt(event.target.value)} placeholder={`What would you like ${agent_name} to do?`} /></label>
      <ImageAttachmentPicker image_count={images.length} limits={DEFAULT_IMAGE_LIMITS} disabled={pending || checking_existing || !images_allowed} on_add={addImages} />
      {image_error ? <p className="workspace-error" role="alert">{image_error}</p> : null}
      {pending ? <p className="workspace-progress" role="status"><span className="inline-spinner" />{snapshot?.status === "stopping" ? `Stopping ${agent_name}…` : `Starting ${agent_name}… Waiting for its conversation to be saved.`}</p> : null}
      {snapshot?.message ? <p className={snapshot.status === "failed" ? "workspace-error" : "workspace-notice"} role={snapshot.status === "failed" ? "alert" : "status"}>{snapshot.message}</p> : null}
      <LastTurnFailure message={snapshot?.last_turn_error} />
      {error || control_error ? <p className="workspace-error" role="alert">{error ?? control_error}</p> : null}
      <footer>
        <button type="button" onClick={on_close}>Close</button>
        {snapshot && !snapshot.session && !["failed", "stopped"].includes(snapshot.status) ? <button type="button" disabled={controlling || snapshot.status === "stopping"} onClick={() => on_stop(snapshot)}>Stop</button> : null}
        {snapshot?.session ? <button type="button" onClick={on_open}>Open conversation</button>
          : <button type="submit" disabled={pending || !checking_existing && (!selected?.available || !can_create || !!images.length && !images_allowed || (!prompt.trim() && !images.length))}>{checking_existing ? "Check session start" : error || snapshot?.status === "failed" || snapshot?.status === "stopped" ? "Try again" : `Start ${agent_name} session`}</button>}
      </footer>
    </form>
  </WorkspaceDialog>;
}

export function ManagedSessionControls({ snapshot, busy, error, on_control }: {
  snapshot: ManagedSessionSnapshot | null; busy: boolean; error: string | null;
  on_control: (kind: "stop" | "resume", snapshot: ManagedSessionSnapshot) => void;
}) {
  if (!snapshot) return null;
  if (snapshot.provider !== "pi") return snapshot.message && (["native_create_uncertain", "first_input_uncertain", "failed"].includes(snapshot.creation_phase ?? "") || snapshot.creation_input_state === "rejected")
    ? <div className="managed-session-controls"><span role="alert" className="workspace-error">{snapshot.message}</span></div> : null;
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
