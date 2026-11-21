import type { AccountIdentity } from "../lib/hubAccount";
import "./HubAccount.css";

export function HubAccountProfile({ username, user_id }: AccountIdentity) {
  return <>
    <div className="account-profile-summary">
      <span className="account-avatar" aria-hidden="true">{Array.from(username)[0]?.toUpperCase()}</span>
      <div><strong className="account-username">{username}</strong><p>Your Hub account</p></div>
    </div>
    <details className="account-profile-details"><summary>Account details</summary>
      <dl><dt>Account ID</dt><dd><code>{user_id}</code></dd></dl>
    </details>
  </>;
}
