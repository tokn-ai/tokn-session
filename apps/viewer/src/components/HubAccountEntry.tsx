import { useRef, type RefObject } from "react";
import type { AccountIdentity } from "../lib/hubAccount";
import { HubAccountMenu } from "./HubAccountMenu";
import { HubPageLink } from "./HubNavigation";
import { SettingsEntry } from "./SettingsEntry";

export type AccountAction = (return_focus: RefObject<HTMLElement | null>) => void;

interface HubAccountEntryProps {
  session?: AccountIdentity;
  signing_out?: boolean;
  sign_out_error?: string;
  on_sign_out: () => void;
  on_sign_in?: AccountAction;
  on_profile?: AccountAction;
}

export function HubAccountEntry({
  session, signing_out = false, sign_out_error, on_sign_out, on_sign_in, on_profile,
}: HubAccountEntryProps) {
  const sign_in = useRef<HTMLButtonElement>(null);
  if (session) return <HubAccountMenu key={session.user_id} username={session.username}
    signing_out={signing_out} sign_out_error={sign_out_error} on_sign_out={on_sign_out} on_profile={on_profile} />;

  return <>
    <SettingsEntry />
    {on_sign_in
      ? <button ref={sign_in} type="button" className="hub-sign-in" aria-haspopup="dialog"
        onClick={() => on_sign_in(sign_in)}>Sign in</button>
      : <HubPageLink className="hub-sign-in" href="/account">Sign in</HubPageLink>}
  </>;
}
