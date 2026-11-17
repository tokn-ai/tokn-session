import { Fragment, useLayoutEffect, useRef, type KeyboardEvent, type RefObject } from "react";
import "./VerificationCodeInput.css";

export function VerificationCodeInput({ kind, value, disabled, input_ref, on_change }: {
  kind: "email" | "device";
  value: readonly string[];
  disabled: boolean;
  input_ref: RefObject<HTMLInputElement | null>;
  on_change: (value: string[]) => void;
}) {
  const device = kind === "device";
  const length = device ? 8 : 6;
  const id = device ? "account-device-code" : "account-email-code";
  const inputs = useRef<(HTMLInputElement | null)[]>([]);
  const pending_focus = useRef<number | undefined>(undefined);

  useLayoutEffect(() => {
    if (pending_focus.current === undefined) return;
    focus(pending_focus.current);
    pending_focus.current = undefined;
  }, [value]);

  function focus(index: number) {
    const input = inputs.current[index];
    input?.focus();
    input?.select();
  }

  function replace(index: number, text: string) {
    const characters = device ? text.replace(/[^A-Za-z0-9]/g, "").toUpperCase() : text.replace(/\D/g, "");
    if (!characters) return;
    // Autofill and a complete pasted code replace every slot, even from the middle.
    const start = characters.length >= length ? 0 : index;
    const inserted = characters.slice(0, length - start);
    const next = Array.from({ length }, (_, slot) => value[slot] ?? "");
    for (let offset = 0; offset < inserted.length; offset++) next[start + offset] = inserted[offset];
    pending_focus.current = Math.min(start + inserted.length, length - 1);
    on_change(next);
  }

  function clear(index: number) {
    on_change(Array.from({ length }, (_, slot) => slot === index ? "" : value[slot] ?? ""));
  }

  function keyDown(event: KeyboardEvent<HTMLInputElement>, index: number) {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    switch (event.key) {
      case "Backspace": {
        event.preventDefault();
        const target = value[index] ? index : Math.max(0, index - 1);
        pending_focus.current = target;
        clear(target);
        break;
      }
      case "Delete": event.preventDefault(); clear(index); break;
      case "ArrowLeft": event.preventDefault(); focus(Math.max(0, index - 1)); break;
      case "ArrowRight": event.preventDefault(); focus(Math.min(length - 1, index + 1)); break;
      case "Home": event.preventDefault(); focus(0); break;
      case "End": event.preventDefault(); focus(length - 1); break;
    }
  }

  return <fieldset className={`verification-code-input verification-code-input--${kind}`} disabled={disabled}
    aria-describedby={device ? undefined : "account-description"}>
    <legend>{device ? "CLI device code" : "Email code"}</legend>
    <div className="verification-code-input__slots">
      {Array.from({ length }, (_, index) => <Fragment key={index}>
        {device && index === 4 && <span className="verification-code-input__separator" aria-hidden="true">–</span>}
        <input
          ref={(element) => {
            inputs.current[index] = element;
            if (index === 0) input_ref.current = element;
          }}
          id={index === 0 ? id : `${id}-${index + 1}`}
          aria-label={`${device ? "Character" : "Digit"} ${index + 1} of ${length}`}
          inputMode={device ? "text" : "numeric"}
          autoCapitalize={device ? "characters" : "none"}
          autoComplete={!device && index === 0 ? "one-time-code" : "off"}
          spellCheck={false}
          required
          pattern={device ? "[A-Z0-9]" : "[0-9]"}
          maxLength={length}
          value={value[index] ?? ""}
          onFocus={(event) => event.currentTarget.select()}
          onClick={(event) => event.currentTarget.select()}
          onChange={(event) => {
            if (event.target.value === "") clear(index);
            else replace(index, event.target.value);
          }}
          onPaste={(event) => {
            event.preventDefault();
            replace(index, event.clipboardData.getData("text"));
          }}
          onKeyDown={(event) => keyDown(event, index)}
        />
      </Fragment>)}
    </div>
  </fieldset>;
}
