import type { ReactNode, SVGProps } from "react";

type IconProps = SVGProps<SVGSVGElement>;

function IconBase({ children, ...props }: IconProps & { children: ReactNode }) {
  return (
    <svg
      aria-hidden="true"
      fill="none"
      height="18"
      viewBox="0 0 24 24"
      width="18"
      {...props}
    >
      {children}
    </svg>
  );
}

export function SearchIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <circle cx="11" cy="11" r="6.5" stroke="currentColor" strokeWidth="1.7" />
      <path d="m16 16 4 4" stroke="currentColor" strokeLinecap="round" strokeWidth="1.7" />
    </IconBase>
  );
}

export function PanelIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <rect height="17" rx="2" stroke="currentColor" strokeWidth="1.7" width="19" x="2.5" y="3.5" />
      <path d="M9 4v16" stroke="currentColor" strokeWidth="1.7" />
    </IconBase>
  );
}

export function InspectorIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <rect height="17" rx="2" stroke="currentColor" strokeWidth="1.7" width="19" x="2.5" y="3.5" />
      <path d="M15 4v16" stroke="currentColor" strokeWidth="1.7" />
    </IconBase>
  );
}

export function FilterIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M4 7h16M7 12h10M10 17h4" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" />
    </IconBase>
  );
}

export function ChevronIcon({ className, ...props }: IconProps) {
  return (
    <IconBase className={className} {...props}>
      <path d="m8.5 10 3.5 3.5 3.5-3.5" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="1.8" />
    </IconBase>
  );
}

export function CloseIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="m7 7 10 10M17 7 7 17" stroke="currentColor" strokeLinecap="round" strokeWidth="1.8" />
    </IconBase>
  );
}

export function ReasoningIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M9 18h6M10 21h4M8.2 14.8a7 7 0 1 1 7.6 0c-.8.6-1.3 1.3-1.5 2.2H9.7c-.2-.9-.7-1.6-1.5-2.2Z" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="1.6" />
    </IconBase>
  );
}

export function UsageIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M5 19V12m7 7V5m7 14v-9" stroke="currentColor" strokeLinecap="round" strokeWidth="1.8" />
      <path d="M3.5 20.5h17" stroke="currentColor" strokeLinecap="round" strokeWidth="1.6" />
    </IconBase>
  );
}

export function ToolIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M14.7 6.2a4.5 4.5 0 0 0-5.8 5.9L3.5 17.5a2.1 2.1 0 0 0 3 3l5.4-5.4a4.5 4.5 0 0 0 5.9-5.8l-2.7 2.6-3-3 2.6-2.7Z" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="1.6" />
    </IconBase>
  );
}

export function TechnicalIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M12 3v4m0 10v4M3 12h4m10 0h4M5.6 5.6l2.8 2.8m7.2 7.2 2.8 2.8m0-12.8-2.8 2.8m-7.2 7.2-2.8 2.8" stroke="currentColor" strokeLinecap="round" strokeWidth="1.6" />
      <circle cx="12" cy="12" r="4" stroke="currentColor" strokeWidth="1.6" />
    </IconBase>
  );
}

export function WarningIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M10.4 4.6 2.9 18a1.7 1.7 0 0 0 1.5 2.5h15.2a1.7 1.7 0 0 0 1.5-2.5L13.6 4.6a1.8 1.8 0 0 0-3.2 0Z" stroke="currentColor" strokeLinejoin="round" strokeWidth="1.6" />
      <path d="M12 9v4.5m0 3.2v.1" stroke="currentColor" strokeLinecap="round" strokeWidth="1.8" />
    </IconBase>
  );
}

export function BellIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M18 10.5a6 6 0 0 0-12 0c0 6-2.5 6.8-2.5 7.8h17c0-1-2.5-1.8-2.5-7.8Z" stroke="currentColor" strokeLinejoin="round" strokeWidth="1.6" />
      <path d="M9.5 21h5" stroke="currentColor" strokeLinecap="round" strokeWidth="1.6" />
    </IconBase>
  );
}

export function MoreIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <circle cx="5" cy="12" fill="currentColor" r="1.3" />
      <circle cx="12" cy="12" fill="currentColor" r="1.3" />
      <circle cx="19" cy="12" fill="currentColor" r="1.3" />
    </IconBase>
  );
}

export function SendIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M12 19V5m-6 6 6-6 6 6" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="1.8" />
    </IconBase>
  );
}

export function BranchIcon(props: IconProps) {
  return (
    <IconBase {...props}>
      <path d="M7 6v7a4 4 0 0 0 4 4h5m-3-3 3 3-3 3" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="1.7" />
      <circle cx="7" cy="4" r="2" stroke="currentColor" strokeWidth="1.7" />
    </IconBase>
  );
}
