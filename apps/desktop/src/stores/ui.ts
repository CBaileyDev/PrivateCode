/** Shared UI-state store: modal + layout + theme signals, so any component (the
 * header, the command palette, the input bar) can drive them without
 * prop-drilling. */
import { createSignal } from "solid-js";

const [settingsOpen, setSettingsOpen] = createSignal(false);
const [sidebarOpen, setSidebarOpen] = createSignal(true);
const [rightPanelOpen, setRightPanelOpen] = createSignal(true);
const [theme, setTheme] = createSignal<"dark" | "light">("dark");

const openSettings = () => setSettingsOpen(true);
const closeSettings = () => setSettingsOpen(false);

const toggleSidebar = () => setSidebarOpen((v) => !v);
const toggleRightPanel = () => setRightPanelOpen((v) => !v);

/** Apply the current theme to the document root (call on mount + on change). */
function applyTheme(t: "dark" | "light") {
  setTheme(t);
  try {
    document.documentElement.setAttribute("data-theme", t);
  } catch {
    /* non-DOM (tests) — best-effort */
  }
}
const toggleTheme = () => applyTheme(theme() === "dark" ? "light" : "dark");

export {
  settingsOpen,
  setSettingsOpen,
  openSettings,
  closeSettings,
  sidebarOpen,
  setSidebarOpen,
  toggleSidebar,
  rightPanelOpen,
  setRightPanelOpen,
  toggleRightPanel,
  theme,
  applyTheme,
  toggleTheme,
};
