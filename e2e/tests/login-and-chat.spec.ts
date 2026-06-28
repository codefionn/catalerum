import { test, expect } from "@playwright/test";

// Login-and-chat smoke test (SOUL §17): redeem a dev magic-link as JSON, drop
// the session token into the SPA URL the way the magic-link redirect does, load
// the workbench, send one chat message, and assert it is echoed back.
//
// The dev magic-link token is one-time, so we mint a fresh one per run by
// hitting the API's dev-login seed indirectly: the API prints the link on boot,
// but for an automated run we read it from the API. Here we accept it via the
// CATALERUM_MAGIC_TOKEN env var (the harness extracts it from the API log) to
// keep the spec hermetic; if unset, the test is skipped with a clear note.

const API = process.env.CATALERUM_API_URL ?? "http://localhost:8787";

test("dev magic-link login lands signed in and chat echoes", async ({ page, request }) => {
  const magicToken = process.env.CATALERUM_MAGIC_TOKEN;
  test.skip(!magicToken, "set CATALERUM_MAGIC_TOKEN (from the API boot log) to run this spec");

  // Redeem the one-time magic token as JSON to get a session (M1 contract).
  const res = await request.get(`${API}/auth/magic?token=${magicToken}&format=json`);
  expect(res.ok()).toBeTruthy();
  const session = await res.json();
  expect(session.token).toBeTruthy();

  // The SPA reads ?token= from its own URL and caches it (single-click login).
  await page.goto(`/?token=${session.token}`);

  // The chat panel should be present once signed in.
  const input = page.getByRole("textbox").first();
  await expect(input).toBeVisible();

  // Send a message; the dev echo llmleaf echoes it back.
  const msg = `e2e-ping-${Date.now()}`;
  await input.fill(msg);
  await input.press("Enter");

  await expect(page.getByText(msg)).toBeVisible();
});
