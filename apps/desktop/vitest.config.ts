import { defineConfig } from "vitest/config";

// Pure-logic tests run in the node environment (no jsdom needed). C15 adds the
// jsdom + solid component/store suite, which will extend this config.
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
