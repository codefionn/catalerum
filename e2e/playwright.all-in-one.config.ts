import { defineConfig, devices } from "@playwright/test";

// Browser tests for the packaged single-container distribution. The lifecycle
// is owned by `just e2e-all-in-one`: every run receives empty persistent
// volumes, so the suite can exercise the real first-boot owner flow.
export default defineConfig({
  testDir: "./all-in-one",
  timeout: 90_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: "list",
  use: {
    baseURL:
      process.env.CATALERUM_ALL_IN_ONE_URL ?? "http://127.0.0.1:18080",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
  },
  projects: [
    { name: "all-in-one-chromium", use: { ...devices["Desktop Chrome"] } },
  ],
});
