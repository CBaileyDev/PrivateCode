/**
 * ThemeToggle — Dark/light mode toggle button.
 */
import { createSignal } from "solid-js";

export default function ThemeToggle() {
  const [isDark, setIsDark] = createSignal(true);

  const toggle = () => {
    const next = !isDark();
    setIsDark(next);
    document.documentElement.setAttribute("data-theme", next ? "dark" : "light");
  };

  return (
    <button
      class="theme-toggle"
      onClick={toggle}
      data-tooltip={isDark() ? "Switch to light mode" : "Switch to dark mode"}
      id="theme-toggle"
    >
      {isDark() ? "☀" : "🌙"}
    </button>
  );
}
