/** Small UI-state store shared across components (e.g. the Settings modal),
 * so a button anywhere can open Settings without prop-drilling. */
import { createSignal } from "solid-js";

const [settingsOpen, setSettingsOpen] = createSignal(false);

const openSettings = () => setSettingsOpen(true);
const closeSettings = () => setSettingsOpen(false);

export { settingsOpen, setSettingsOpen, openSettings, closeSettings };
