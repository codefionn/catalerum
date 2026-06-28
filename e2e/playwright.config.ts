import { defineConfig, devices } from "@playwright/test";

// Minimal Playwright config for the catalerum login-and-chat smoke test.
//
// Assumes the stack is already up (`just up`) and the API + web workbench are
// serving (`just dev` + `just web`). Override origins via env:
//   CATALERUM_WEB_URL (default http://localhost:8080)
//   CATALERUM_API_URL (default http://localhost:8787)
//
// Run: cd e2e && npm install && npx playwright install chromium && npx playwright test
export default defineConfig({
  testDir: "./tests",
  timeout: 30_000,
  expect: { timeout: 10_000 },
  fullyParallel: false,
  reporter: "list",
  use: {
    baseURL: process.env.CATALERUM_WEB_URL ?? "http://localhost:8080",
    trace: "retain-on-failure",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
  ],
});
