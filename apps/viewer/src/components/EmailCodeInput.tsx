import { useLayoutEffect, useRef, type KeyboardEvent, type RefObject } from "react";
import "./EmailCodeInput.css";

const CODE_LENGTH = 6;

export function EmailCodeInput({ value, disabled, input_ref, on_change }: {
  value: readonly string[];
  disabled: boolean;
  input_ref: RefObject<HTMLInputElement | null>;
  on_change: (value: string[]) => void;
}) {
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
    const digits = text.replace(/\D/g, "");
    if (!digits) return;
    // Autofill and a complete pasted code replace every slot, even from the middle.
    const start = digits.length >= CODE_LENGTH ? 0 : index;
    const inserted = digits.slice(0, CODE_LENGTH - start);
    const next = Array.from({ length: CODE_LENGTH }, (_, slot) => value[slot] ?? "");
    for (let offset = 0; offset < inserted.length; offset++) next[start + offset] = inserted[offset];
    pending_focus.current = Math.min(start + inserted.length, CODE_LENGTH - 1);
    on_change(next);
  }

  function clear(index: number) {
    on_change(Array.from({ length: CODE_LENGTH }, (_, slot) => slot === index ? "" : value[slot] ?? ""));
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
      case "ArrowRight": event.preventDefault(); focus(Math.min(CODE_LENGTH - 1, index + 1)); break;
      case "Home": event.preventDefault(); focus(0); break;
      case "End": event.preventDefault(); focus(CODE_LENGTH - 1); break;
    }
  }

  return <fieldset className="email-code-input" disabled={disabled} aria-describedby="account-description">
    <legend>Email code</legend>
    <div className="email-code-input__digits">
      {Array.from({ length: CODE_LENGTH }, (_, index) => <input
        key={index}
        ref={(element) => {
          inputs.current[index] = element;
          if (index === 0) input_ref.current = element;
        }}
        id={index === 0 ? "account-email-code" : `account-email-code-${index + 1}`}
        aria-label={`Digit ${index + 1} of ${CODE_LENGTH}`}
        inputMode="numeric"
        autoComplete={index === 0 ? "one-time-code" : "off"}
        spellCheck={false}
        required
        pattern="[0-9]"
        maxLength={CODE_LENGTH}
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
      />)}
    </div>
  </fieldset>;
}
