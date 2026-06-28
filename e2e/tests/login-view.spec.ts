import { test, expect } from "@playwright/test";
import { API, resolveSession, loginWith } from "./helpers";

// The unauthenticated sign-in surface (SOUL §12/§18, landed 2026-07-02):
//   - `App` first tries to adopt an inbound `?token=`; with none, and nothing in
//     localStorage, it mounts `LoginView` (before this landed the app just mounted
//     the Workbench and every panel silently 401'd).
//   - `LoginView` is a centred `.wb-login-card`: the app name/brand, an optional
//     "Sign in with SSO" button (shown unless the anonymous probe says `sso:false`),
//     and a dev magic-link hint. It probes the **anonymous** `GET /status/login`
//     (the authed `GET /status` 401s here) for the `{sso, mode}` presentation flags.
//   - Adopting a `?token=` swaps in the Workbench and scrubs *only* the `token`
//     param from the address bar (path/hash/other params preserved, no reload).
//
// NB: `.wb-title` ("catalerum") renders in BOTH the login card and the workbench
// header, so it can't tell the two states apart — the login card is identified by
// `.wb-login-card` / `.wb-login-hint`, the workbench by `.wb-header` + `.wb-nav`
// (which never appear on the login card).

test("with no token the login card renders and /status/login is anonymous", async ({
  page,
  request,
}) => {
  // A fresh Playwright context ⇒ empty localStorage, and no `?token=` in the URL,
  // so `App` resolves no session and mounts `LoginView`. (This test needs no bearer.)
  await page.goto("/");

  // The centred sign-in card: brand + the always-present dev magic-link hint.
  const card = page.locator(".wb-login-card");
  await expect(card).toBeVisible();
  await expect(card.locator(".wb-title")).toHaveText("catalerum");
  await expect(card.locator(".wb-subtitle")).toHaveText("a catalogue of things");
  await expect(page.locator(".wb-login-hint")).toContainText("magic-link");

  // It is the login surface, not the workbench: no header/nav chrome.
  await expect(page.locator(".wb-nav")).toHaveCount(0);
  await expect(page.locator(".wb-header")).toHaveCount(0);

  // The button seam is intentionally NOT asserted: it shows unless the probe
  // positively reports `sso:false`. On the seeded (non-SSO) dev instance the probe
  // resolves `sso:false` so the button hides — but that is instance-dependent, so
  // we assert the card, per the task, not the button.

  // `GET /status/login` is the anonymous presentation slice: a plain (no-auth) fetch
  // must succeed and carry the two non-secret flags the login view keys off.
  const res = await request.get(`${API}/status/login`);
  expect(res.status(), `GET /status/login -> ${res.status()}`).toBe(200);
  const body = (await res.json()) as { sso?: unknown; mode?: unknown };
  expect(typeof body.sso).toBe("boolean");
  expect(typeof body.mode).toBe("string");
  // On the seeded dev stack this is the single_user, non-SSO deployment.
  expect(body.mode).toBe("single_user");
});

test("adopting a ?token= mounts the workbench and scrubs the token from the URL", async ({
  page,
  request,
}) => {
  const token = await resolveSession(request);
  test.skip(
    !token,
    "set CATALERUM_DEV_AUTHORIZATION_TOKEN (stable dev bearer) to run this spec",
  );
  const bearer = token as string;

  // Land with `?token=` on a deep path — the SPA adopts the bearer, mounts the
  // workbench, and rewrites the address bar. `loginWith` waits for the signed-in
  // header title.
  await loginWith(page, bearer);

  // The workbench is up (its header + nav render), and the login card is gone.
  await expect(page.locator(".wb-header")).toBeVisible();
  await expect(page.locator(".wb-nav")).toBeVisible();
  await expect(page.locator(".wb-login-card")).toHaveCount(0);

  // The token param was scrubbed from the URL (no reload; path preserved as "/").
  await expect
    .poll(() => new URL(page.url()).searchParams.get("token"), {
      message: "the ?token= param should be stripped after adoption",
    })
    .toBeNull();
  expect(new URL(page.url()).pathname).toBe("/");
});
