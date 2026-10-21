import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: { alias: { "@": import.meta.dirname } },
  test: { include: ["tests/**/*.test.{ts,tsx}"], testTimeout: 30000 },
});
