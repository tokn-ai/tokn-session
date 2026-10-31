import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { consumeBootstrapToken, consumeLoginToken } from "./lib/login";
import { installViewportSizing } from "./lib/viewport";

installViewportSizing();
const initial_token = consumeLoginToken();
const bootstrap_token = consumeBootstrapToken();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App initial_token={initial_token} bootstrap_token={bootstrap_token} />
  </React.StrictMode>,
);
