import React from "react";
import ReactDOM from "react-dom/client";
import "./styles.css";
import App from "./App";
import { followOmarchyTheme } from "./omarchy";

followOmarchyTheme().catch(() => {
  // keep t3boat's own look
});

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
