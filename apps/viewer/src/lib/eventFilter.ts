import type { EventSummary } from "./types";

/** Filter only known routine records explicitly classified by the backend. */
export function isBookkeepingEvent(event: EventSummary): boolean {
  if (event.is_bookkeeping !== true || event.is_error === true) return false;
  switch (event.type) {
    case "session_started":
    case "provider_changed":
    case "session_settings_applied":
    case "lifecycle":
    case "metadata":
    case "usage":
    case "activity_group":
      return true;
    default:
      // Content, outcomes, and unfamiliar provider events stay visible
      // even if an incompatible server accidentally classifies them otherwise.
      return false;
  }
}
