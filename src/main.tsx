import ReactDOM from "react-dom/client";
import App from "./App";
import "./index.css";
import { initLatencyProbe } from "./lib/latencyProbe";

// StrictMode intentionally omitted: its double-invoke of effects would spawn
// two PTYs per pane in dev. Re-introduce with a spawn guard in Phase 1.
//
// Keitora is a desktop-only app: the full board with embedded PTY terminals.

void initLatencyProbe();
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);
