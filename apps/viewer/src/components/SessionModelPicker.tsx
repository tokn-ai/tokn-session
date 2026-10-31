import { useCallback, useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useFloatingPanel } from "../lib/useFloatingPanel";
import { modelLabel, sameModel } from "../lib/sessionModels";
import type { SessionModelController } from "../lib/useSessionModel";
import type { ModelChoice } from "../lib/types";
import { ChevronIcon, CloseIcon } from "./Icons";
import "./SessionModelPicker.css";

export function SessionModelPicker({ model, provider, input_busy = false, selected_model = null, on_select, on_clear }: {
  model: SessionModelController; provider: string; input_busy?: boolean;
  selected_model?: ModelChoice | null;
  on_select: (choice: ModelChoice) => void;
  on_clear: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ left: 16, bottom: 80, maxHeight: 460 });
  const trigger = useRef<HTMLButtonElement>(null);
  const panel_id = useId();
  const close = useCallback(() => setOpen(false), []);
  const panel = useFloatingPanel(open, close, trigger);
  const identity = selected_model?.model ?? model.state?.model;
  const label = identity ? modelLabel(identity) : provider;
  const reason = model.reason ?? model.error ?? (model.checking ? "Checking model availability…" : "Model selection is unavailable for this session.");

  useEffect(() => {
    if (!open) return;
    const place = () => {
      const rect = trigger.current?.getBoundingClientRect();
      const visible_top = window.visualViewport?.offsetTop ?? 0;
      if (rect) setPosition({ left: Math.max(12, Math.min(rect.left, window.innerWidth - 332)),
        bottom: Math.max(12, window.innerHeight - rect.top + 8), maxHeight: Math.min(460, Math.max(80, rect.top - visible_top - 24)) });
    };
    place();
    window.addEventListener("resize", place);
    window.visualViewport?.addEventListener("resize", place);
    window.visualViewport?.addEventListener("scroll", place);
    return () => {
      window.removeEventListener("resize", place);
      window.visualViewport?.removeEventListener("resize", place);
      window.visualViewport?.removeEventListener("scroll", place);
    };
  }, [open]);

  useEffect(() => {
    if (open && model.choices === null && !model.loading && !model.error) void model.loadChoices();
  }, [open, model.choices, model.loading, model.error, model.loadChoices]);

  return <div className="session-model">
    {model.can_open ? <button ref={trigger} className="session-model__trigger" type="button"
      aria-label={`Model: ${label}. Change model`} aria-haspopup="dialog" aria-expanded={open} aria-controls={panel_id}
      disabled={input_busy} onClick={() => setOpen((value) => !value)}>
      <span>{label}</span><ChevronIcon aria-hidden="true" />
    </button> : <span className="session-model__static" title={reason}>
      <span className="session-model__name">{label}</span>{!selected_model && identity && (model.state?.source === "recorded" || model.state?.source === "catalog")
        ? <span className="session-model__recorded">{model.state.source === "recorded" ? "Recorded" : "Catalog"}</span> : null}
    </span>}
    {selected_model ? <button className="text-button session-model__retry" type="button" disabled={input_busy} onClick={on_clear}>Use current model</button> : null}
    {!model.can_open && !model.checking ? <small className="session-model__reason" title={reason}>{reason}</small> : null}
    {!model.can_open && !model.checking && model.error ? <button className="text-button session-model__retry" type="button" onClick={() => { void model.refresh(); }}>Refresh model status</button> : null}
    {open ? createPortal(<div className="session-model__panel" ref={panel} id={panel_id} role="dialog" aria-modal="false"
      aria-labelledby={`${panel_id}-title`} tabIndex={-1} style={position}>
      <header><div><h3 id={`${panel_id}-title`}>Model</h3><p>{provider}</p></div>
        <button className="icon-button" type="button" aria-label="Close model selection" onClick={close}><CloseIcon /></button></header>
      {model.loading || model.checking ? <p className="session-model__notice" role="status"><span className="inline-spinner" />Loading available models…</p> : null}
      {model.error ? <div className="session-model__error" role="alert"><p>{model.error}</p>
        <button type="button" className="text-button" disabled={model.loading || model.checking}
          onClick={() => { if (!model.status) void model.refresh(); else void model.loadChoices(); }}>
          {!model.status ? "Refresh model status" : "Retry models"}</button></div> : null}
      {model.choices?.length ? <div className="session-model__choices" role="group" aria-label="Available models">
        {model.choices.map((choice) => {
          const selected = sameModel(identity, choice.model);
          return <button key={JSON.stringify([choice.model.model_provider, choice.model.model_id])} type="button"
            aria-pressed={selected} className="session-model__choice" disabled={!model.can_select || input_busy || selected}
            onClick={() => { on_select(choice); }}>
            <span><strong>{modelLabel(choice.model)}</strong>{" "}<small>{choice.model.model_provider ?? choice.model.model_id}{choice.supports_images ? " · Images" : ""}</small></span>
            <span aria-hidden="true">{selected ? "✓" : ""}</span>
          </button>;
        })}
      </div> : model.choices && !model.loading ? <p className="session-model__notice">No verified models are available.</p> : null}
      {!model.can_select && !model.checking && !model.loading ? <p className="session-model__notice">{reason}</p> : null}
      <p className="session-model__footnote">{model.running
        ? "While this session is responding, Send steers the current model and ignores this choice."
        : "Your model choice is applied when you send the next message."}</p>
      {model.status?.updates_pi_default ? <p className="session-model__footnote">When applied on Send, this choice also updates Pi’s default for new sessions on this machine.</p> : null}
    </div>, document.body) : null}
  </div>;
}
