import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
// theme.css must be imported before any component modules (see the note at
// the top of that file about `.infinity-ui-root` reset ordering).
import "infinity-ui/src/theme.css";
import { App } from "./App";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
