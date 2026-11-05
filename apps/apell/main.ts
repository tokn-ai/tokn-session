import { applet } from "@apell/sdk";
import { mountApellViewer } from "../viewer/src/apell";

const root = document.getElementById("root");
if (!root) throw new Error("Missing viewer root.");

mountApellViewer({
  invoke: (command, params) => applet.invoke(command, params),
  onEvent: (handler) => applet.onEvent(handler),
}, root);
