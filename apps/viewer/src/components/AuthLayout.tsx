import type { ReactNode } from "react";
import "./AuthLayout.css";

export function AuthLayout({ children, navigation, footer }: {
  children: ReactNode;
  navigation?: ReactNode;
  footer?: ReactNode;
}) {
  return <main className="auth-layout">
    <header className="auth-header">
      <div className="auth-brand" aria-label="Tokn">
        <span className="brand-mark" aria-hidden="true"><span /><span /><span /></span>
        <span>tokn<span className="auth-brand-dot">.</span></span>
      </div>
      {navigation && <div className="auth-navigation">{navigation}</div>}
    </header>
    <div className="auth-content">
      <div className="auth-card">{children}</div>
      {footer && <p className="auth-footer">{footer}</p>}
    </div>
  </main>;
}
