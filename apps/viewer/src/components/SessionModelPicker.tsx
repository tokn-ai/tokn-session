import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties } from "react";
import { createPortal } from "react-dom";
import { useViewerVisible } from "../lib/viewerVisibility";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { modelLabel, sameModel } from "../lib/sessionModels";
import type { ModelChoice, SessionModelState, SessionModelStatus } from "../lib/types";
import { ChevronIcon, CloseIcon } from "./Icons";
import "./SessionModelPicker.css";

interface ModelPickerController {
  state: SessionModelState | null;
  status?: Pick<SessionModelStatus, "updates_pi_default" | "model_input_mode"> | null;
  reason?: string | null;
  error: string | null;
  checking: boolean;
  loading: boolean;
  choices: ModelChoice[] | null;
  can_open: boolean;
  can_select: boolean;
  running?: boolean;
  refresh: () => Promise<void>;
  loadChoices: () => Promise<void>;
}

export function SessionModelPicker({ model, provider, input_busy = false, selected_model = null, selection_current = true, mode = "session", on_select, on_clear }: {
  model: ModelPickerController; provider: string; input_busy?: boolean; mode?: "session" | "creation";
  selected_model?: ModelChoice | null;
  selection_current?: boolean;
  on_select: (choice: ModelChoice) => void;
  on_clear: () => void;
}) {
  const visible = useViewerVisible();
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState<CSSProperties>({});
  const trigger = useRef<HTMLButtonElement>(null);
  const panel_id = useId();
  const close = useCallback(() => setOpen(false), []);
  const panel = useFloatingPanel(open, close, trigger);
  const identity = selected_model?.model ?? model.state?.model;
  const label = identity ? modelLabel(identity) : mode === "creation" ? "Runtime default" : provider;
  const reason = model.reason ?? model.error ?? (model.checking ? "Checking model availability…" : "Model selection is unavailable for this session.");

  useLayoutEffect(() => {
    if (!open) return;
    const place = (event?: Event) => {
      if (event?.target instanceof Node && panel.current?.contains(event.target)) return;
      const rect = trigger.current?.getBoundingClientRect();
      if (!rect) return;
      const viewport = window.visualViewport;
      const visible_top = viewport?.offsetTop ?? 0;
      const visible_bottom = visible_top + (viewport?.height ?? window.innerHeight);
      const anchor_top = Math.min(visible_bottom, rect.top);
      const anchor_bottom = Math.max(visible_top, rect.bottom);
      const space_above = anchor_top - visible_top - 24;
      const space_below = visible_bottom - anchor_bottom - 24;
      const above = space_above >= 200 || space_above >= space_below;
      setPosition({
        left: Math.max(12, Math.min(rect.left, window.innerWidth - 332)),
        ...(above ? { bottom: window.innerHeight - anchor_top + 8 } : { top: anchor_bottom + 8 }),
        maxHeight: Math.max(0, Math.min(460, above ? space_above : space_below)),
      });
    };
    place();
    document.addEventListener("scroll", place, true);
    window.addEventListener("resize", place);
    window.visualViewport?.addEventListener("resize", place);
    window.visualViewport?.addEventListener("scroll", place);
    return () => {
      document.removeEventListener("scroll", place, true);
      window.removeEventListener("resize", place);
      window.visualViewport?.removeEventListener("resize", place);
      window.visualViewport?.removeEventListener("scroll", place);
    };
  }, [open, panel]);

  useEffect(() => {
    if (open && mode === "session" && model.choices === null && !model.loading && !model.error) void model.loadChoices();
  }, [open, mode, model.choices, model.loading, model.error, model.loadChoices]);

  useEffect(() => {
    // Opening a draft picker also recovers a catalog rejected at first Send.
    if (open && mode === "creation") void model.loadChoices();
  }, [open, mode, model.loadChoices]);

  return <div className="session-model">
    {model.can_open ? <button ref={trigger} className="session-model__trigger" type="button"
      aria-label={`Model: ${label}. Change model`} aria-haspopup="dialog" aria-expanded={open} aria-controls={panel_id}
      disabled={input_busy} onClick={() => setOpen((value) => !value)}>
      <span>{label}</span><ChevronIcon aria-hidden="true" />
    </button> : <span className="session-model__static" title={reason}>
      <span className="session-model__name">{label}</span>{mode === "session" && !selected_model && identity && (model.state?.source === "recorded" || model.state?.source === "catalog")
        ? <span className="session-model__recorded">{model.state.source === "recorded" ? "Recorded" : "Model list"}</span> : null}
    </span>}
    {selected_model ? <button className="text-button session-model__retry" type="button" disabled={input_busy} onClick={on_clear}>{mode === "creation" ? "Use runtime default" : "Use current model"}</button> : null}
    {!model.can_open && !model.checking ? <small className="session-model__reason" title={reason}>{reason}</small> : null}
    {!model.can_open && !model.checking && model.error ? <button className="text-button session-model__retry" type="button" onClick={() => { void model.refresh(); }}>Refresh model status</button> : null}
    {open && visible ? createPortal(<div className="session-model__panel" ref={panel} id={panel_id} role="dialog" aria-modal="false"
      aria-labelledby={`${panel_id}-title`} tabIndex={-1} style={position}>
      <header><div><h3 id={`${panel_id}-title`}>Model</h3><p>{provider}</p></div>
        <button className="icon-button" type="button" aria-label="Close model selection" onClick={close}><CloseIcon /></button></header>
      {model.loading || model.checking ? <p className="session-model__notice" role="status"><span className="inline-spinner" />Loading available models…</p> : null}
      {model.error ? <div className="session-model__error" role="alert"><p>{model.error}</p>
        <button type="button" className="text-button" disabled={model.loading || model.checking}
          onClick={() => { if (mode === "session" && !model.status) void model.refresh(); else void model.loadChoices(); }}>
          {mode === "session" && !model.status ? "Refresh model status" : "Retry models"}</button></div> : null}
      {mode === "creation" ? <div className="session-model__choices"><button type="button" className="session-model__choice"
        aria-pressed={!selected_model} disabled={input_busy} onClick={on_clear}>
        <span><strong>Runtime default</strong> <small>Use the agent’s configured model</small></span><span aria-hidden="true">{!selected_model ? "✓" : ""}</span>
      </button></div> : null}
      {model.choices?.length ? <div className="session-model__choices" role="group" aria-label="Available models">
        {model.choices.map((choice) => {
          const selected = selection_current && sameModel(identity, choice.model);
          return <button key={JSON.stringify([choice.model.model_provider, choice.model.model_id])} type="button"
            aria-pressed={selected} className="session-model__choice" disabled={!model.can_select || input_busy || selected}
            onClick={() => { on_select(choice); }}>
            <span><strong>{modelLabel(choice.model)}</strong>{" "}<small>{choice.model.model_provider ?? choice.model.model_id}{choice.supports_images ? " · Images" : ""}</small></span>
            <span aria-hidden="true">{selected ? "✓" : ""}</span>
          </button>;
        })}
      </div> : model.choices && !model.loading ? <p className="session-model__notice">No verified models are available.</p> : null}
      {!model.can_select && !model.checking && !model.loading && !model.error ? <p className="session-model__notice">{reason}</p> : null}
      <p className="session-model__footnote">{mode === "creation" ? "Your model choice is applied when you send the first message."
        : model.status?.model_input_mode === "per_input" ? "Your model choice stays with this message. Guidance can use it in ongoing work; queued follow-ups keep their choice."
        : model.running
        ? "While this session is responding, Send steers the current model and ignores this choice."
        : "Your model choice is applied when you send the next message."}</p>
      {model.status?.updates_pi_default ? <p className="session-model__footnote">When applied on Send, this choice also updates Pi’s default for new sessions on this machine.</p> : null}
    </div>, document.body) : null}
  </div>;
}
