import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";

// Pure-logic store tests run in the node environment (no jsdom). Component tests
// opt into jsdom via a `// @vitest-environment jsdom` docblock. The Solid plugin
// transforms JSX/reactivity for the component mount-smoke tests; the
// `development`/`browser` resolve conditions make solid-js use its dev runtime so
// rendering works under jsdom.
export default defineConfig({
  plugins: [solid()],
  resolve: {
    conditions: ["development", "browser"],
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
