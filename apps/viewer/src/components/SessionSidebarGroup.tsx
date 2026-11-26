import { useEffect, useId, useState, type ReactNode } from "react";
import { ChevronIcon } from "./Icons";

interface SessionSidebarGroupProps {
  title: string;
  count: number;
  selected_key: string | null;
  search: string;
  label?: string;
  action?: ReactNode;
  children: ReactNode;
}

export function SessionSidebarGroup({ title, count, selected_key, search, label, action, children }: SessionSidebarGroupProps) {
  const [expanded, setExpanded] = useState(true);
  const groupId = useId();
  useEffect(() => {
    if (selected_key || search.trim()) setExpanded(true);
  }, [selected_key, search]);

  return <section className="session-group" aria-label={label}>
    <header className="session-group__header">
      <h2>
        <button type="button" className="session-group__toggle" aria-expanded={expanded} aria-controls={groupId}
          title={title} onClick={() => setExpanded(!expanded)}>
          <ChevronIcon /><span className="session-group__label">{title}</span>
          <span className="session-group__count">{count}</span>
        </button>
      </h2>
      {action}
    </header>
    <div id={groupId} className="session-group__items">
      {expanded && children}
    </div>
  </section>;
}
