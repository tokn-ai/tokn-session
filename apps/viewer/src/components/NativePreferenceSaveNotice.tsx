import { nativeMachinePreferences } from "../lib/machinePreferences";
import { useNativePreferenceSaveStatus } from "../lib/nativePreferenceStore";
import { nativeViewerPreferences } from "../lib/viewerPreferences";
import "./NativePreferenceSaveNotice.css";

export function NativePreferenceSaveNotice({ placement }: { placement: "workspace" | "settings" }) {
  const viewer = useNativePreferenceSaveStatus(nativeViewerPreferences);
  const machines = useNativePreferenceSaveStatus(nativeMachinePreferences);
  if (!viewer.error && !machines.error) return null;
  return <div className="native-preferences-notice" data-placement={placement} role="alert">
    <div><strong>Preferences could not be saved</strong>
      <p>Your changes remain available in this window. {viewer.error ?? machines.error}</p>
    </div>
    <button disabled={viewer.saving || machines.saving} onClick={() => {
      if (viewer.error) nativeViewerPreferences.retry();
      if (machines.error) nativeMachinePreferences.retry();
    }}>Retry save</button>
  </div>;
}
