import { useEffect, useState } from "react";

export function CopyTextButton({ text, label, class_name, show_label = false }: {
  text: string; label: string; class_name: string; show_label?: boolean;
}) {
  const [notice, setNotice] = useState("");
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(""), 3000);
    return () => window.clearTimeout(timer);
  }, [notice]);
  if (typeof navigator.clipboard?.writeText !== "function") return null;

  return <>
    <button aria-label={label} className={class_name} title={label} type="button"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          setNotice("Copied");
        } catch {
          setNotice("Copy unavailable");
        }
      }}>
      <svg aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6">
        <rect x="8" y="8" width="12" height="13" rx="2" />
        <path d="M15 5V4a1 1 0 0 0-1-1H5a2 2 0 0 0-2 2v10a1 1 0 0 0 1 1h1" />
      </svg>
      {show_label && "Copy"}
    </button>
    {notice && <span role="status">{notice}</span>}
  </>;
}
