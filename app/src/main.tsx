import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import Editor from "./Editor";
import PromptWindow from "./PromptWindow";
import ConnectBanner from "./ConnectBanner";

const label = getCurrentWindow().label;
const Root = label === "editor" ? Editor : label === "prompt" ? PromptWindow : label === "connect" ? ConnectBanner : App;

// Both windows load the same bundle, so both stylesheets apply everywhere.
// Page-level rules (e.g. body background) are scoped by this attribute so
// the editor's opaque body can't paint over the popup's transparent corners.
document.documentElement.dataset.window = label;

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
