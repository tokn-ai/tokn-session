import type { ReactNode } from "react";
import "./SessionComposer.css";

export function ComposerToolbar({ options, children }: { options: ReactNode; children: ReactNode }) {
  return <div className="session-composer__footer composer-toolbar">
    <div className="composer-toolbar__options" role="group" aria-label="Message options">{options}</div>
    <div className="composer-toolbar__delivery" role="group" aria-label="Send options">{children}</div>
  </div>;
}
