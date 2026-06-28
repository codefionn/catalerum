import { expect, type APIRequestContext, type Page } from "@playwright/test";

// Shared e2e helpers (SOUL §17). These factor the login flow out of
// `login-and-chat.spec.ts` so every spec logs in the same way, and add the small
// amount of REST plumbing the feature specs need (create org/workspace,
// automations, calendar connections) using the admin bearer.
//
// **Session source.** The dev magic-link token is *one-time* (redeeming it
// consumes it), so it can only log in a single spec per stack boot. For a suite
// of independent specs we instead prefer `CATALERUM_DEV_AUTHORIZATION_TOKEN` —
// the stable, reusable session bearer `just dev` seeds (it becomes a real 365-day
// session via `IamService::ensure_dev_authorization_token`, so it is valid both
// as an `Authorization: Bearer` and as the SPA's `?token=`). If only the one-time
// magic token is available we fall back to redeeming it once. If neither is set
// the caller `test.skip`s with a clear note, matching the existing smoke test.

export const API = process.env.CATALERUM_API_URL ?? "http://localhost:8787";

/** A short, unique-per-run suffix so parallel/repeat runs never collide on names. */
export function runSuffix(): string {
  return `${Date.now().toString(36)}-${Math.floor(Math.random() * 1e6).toString(36)}`;
}

/**
 * Resolve a **reusable** session bearer for the signed-in admin.
 *
 * Prefers `CATALERUM_DEV_AUTHORIZATION_TOKEN` (stable, reusable). Falls back to
 * redeeming a one-time `CATALERUM_MAGIC_TOKEN` via `GET /auth/magic?format=json`.
 * Returns `null` when neither is available (the caller should `test.skip`).
 */
export async function resolveSession(
  request: APIRequestContext,
): Promise<string | null> {
  const devToken = process.env.CATALERUM_DEV_AUTHORIZATION_TOKEN?.trim();
  if (devToken) return devToken;

  const magicToken = process.env.CATALERUM_MAGIC_TOKEN?.trim();
  if (magicToken) {
    const res = await request.get(
      `${API}/auth/magic?token=${encodeURIComponent(magicToken)}&format=json`,
    );
    if (res.ok()) {
      const session = await res.json();
      if (session?.token) return session.token as string;
    }
  }
  return null;
}

/**
 * Sign the SPA in with a session `token` and land on the workbench.
 *
 * Mirrors the magic-link redirect the smoke test uses: the SPA reads `?token=`
 * from its own URL and caches it in `localStorage`, so a single-click login. We
 * wait for chrome that ONLY the signed-in workbench renders — its header + nav.
 * (`.wb-title` renders in BOTH the login card and the workbench header, so it
 * can't tell the two states apart; the workbench is identified by `.wb-header` +
 * `.wb-nav`, which never appear on the login card — see `login-view.spec.ts`.)
 */
export async function loginWith(page: Page, token: string): Promise<void> {
  await page.goto(`/?token=${encodeURIComponent(token)}`);
  await expect(page.locator(".wb-header")).toBeVisible();
  await expect(page.locator(".wb-nav")).toBeVisible();
}

/**
 * Deep-link straight to an `/app/*` panel while signing in. Trunk serves the SPA
 * for any `/app/*` path (SPA fallback), the SPA reads `?token=` on any path, and
 * the shell derives the active panel from the URL — so this both authenticates
 * and lands on the panel. Waits for the signed-in workbench chrome (`.wb-header` +
 * `.wb-nav`) — never the ambiguous `.wb-title`, which also renders on the login card.
 */
export async function gotoPanel(
  page: Page,
  token: string,
  appPath: string,
): Promise<void> {
  await page.goto(`${appPath}?token=${encodeURIComponent(token)}`);
  await expect(page.locator(".wb-header")).toBeVisible();
  await expect(page.locator(".wb-nav")).toBeVisible();
}

/** Common headers for authed JSON REST calls. */
export function authHeaders(token: string): Record<string, string> {
  return { Authorization: `Bearer ${token}`, "Content-Type": "application/json" };
}

/**
 * Create an automation via REST (SOUL §11). The visual builder is heavy for e2e,
 * so the automation-affordance spec creates its fixtures here and drives only the
 * UI affordances. `triggers`/`actions` are the raw typed specs the API validates.
 */
export async function createAutomation(
  request: APIRequestContext,
  token: string,
  body: {
    name: string;
    enabled?: boolean;
    // All optional: the server defaults `triggers`/`actions` to `[]`. A **graph**
    // automation carries its whole definition under `spec` (`{graph:{nodes,edges}}`)
    // — the server compiles the dispatch `triggers` from the graph and ignores the
    // linear columns — so the cadence spec passes only `{name, enabled, spec}`.
    triggers?: unknown[];
    actions?: unknown[];
    condition?: unknown;
    spec?: unknown;
  },
): Promise<void> {
  const res = await request.post(`${API}/automations`, {
    headers: authHeaders(token),
    data: { enabled: true, ...body },
  });
  expect(
    res.status(),
    `POST /automations (${body.name}) -> ${res.status()}: ${await res.text()}`,
  ).toBe(201);
}

/** Best-effort delete of an automation (test cleanup; ignores absence). */
export async function deleteAutomation(
  request: APIRequestContext,
  token: string,
  name: string,
): Promise<void> {
  await request
    .delete(`${API}/automations/${encodeURIComponent(name)}`, {
      headers: authHeaders(token),
    })
    .catch(() => undefined);
}

// --- Organisations REST plumbing (SOUL §18) --------------------------------
// The org-admin spec drives affordances through the UI but needs REST for fixture
// setup (create an org / workspace, archive a workspace to trip the delete 409) and
// for the parts the single_user UI can't reach (the members panel + email lookup are
// multi_user-only, so their round-trip is asserted server-side).

/** One workspace inside an organisation, as `GET /organisations` returns it. */
export interface OrgWorkspace {
  id: string;
  name: string;
  slug: string;
  role: string;
}
/** One of the caller's organisations (with the workspaces they can see in it). */
export interface MyOrg {
  id: string;
  name: string;
  slug: string;
  role: string;
  workspaces: OrgWorkspace[];
}

/** `GET /organisations` — the caller's orgs, each with its visible workspaces. */
export async function listOrgs(
  request: APIRequestContext,
  token: string,
): Promise<MyOrg[]> {
  const res = await request.get(`${API}/organisations`, {
    headers: authHeaders(token),
  });
  expect(res.ok(), `GET /organisations -> ${res.status()}`).toBeTruthy();
  return (await res.json()) as MyOrg[];
}

/** `POST /organisations` — create an org (creator becomes Owner). Returns its id. */
export async function createOrg(
  request: APIRequestContext,
  token: string,
  body: { name: string; slug: string },
): Promise<{ id: string; name: string; slug: string }> {
  const res = await request.post(`${API}/organisations`, {
    headers: authHeaders(token),
    data: body,
  });
  expect(
    res.status(),
    `POST /organisations (${body.slug}) -> ${res.status()}: ${await res.text()}`,
  ).toBe(201);
  return (await res.json()) as { id: string; name: string; slug: string };
}

/** Best-effort `DELETE /organisations/{id}` (cleanup; ignores 404/409). */
export async function deleteOrg(
  request: APIRequestContext,
  token: string,
  orgId: string,
): Promise<void> {
  await request
    .delete(`${API}/organisations/${orgId}`, { headers: authHeaders(token) })
    .catch(() => undefined);
}

/** `POST /organisations/{id}/workspaces` — create a workspace in an org. */
export async function createOrgWorkspace(
  request: APIRequestContext,
  token: string,
  orgId: string,
  body: { name: string; slug: string },
): Promise<{ id: string; name: string; slug: string }> {
  const res = await request.post(`${API}/organisations/${orgId}/workspaces`, {
    headers: authHeaders(token),
    data: body,
  });
  expect(
    res.status(),
    `POST /organisations/${orgId}/workspaces -> ${res.status()}: ${await res.text()}`,
  ).toBe(201);
  return (await res.json()) as { id: string; name: string; slug: string };
}

/** Best-effort soft-archive of a workspace shell (`DELETE …/workspaces/{ws}`). */
export async function archiveWorkspace(
  request: APIRequestContext,
  token: string,
  orgId: string,
  wsId: string,
): Promise<void> {
  await request
    .delete(`${API}/organisations/${orgId}/workspaces/${wsId}`, {
      headers: authHeaders(token),
    })
    .catch(() => undefined);
}

/** `GET /organisations/{id}/members` — the org's members (org admin/owner only). */
export async function listOrgMembers(
  request: APIRequestContext,
  token: string,
  orgId: string,
): Promise<Array<{ user_id: string; email: string; display_name: string; role: string }>> {
  const res = await request.get(`${API}/organisations/${orgId}/members`, {
    headers: authHeaders(token),
  });
  expect(
    res.ok(),
    `GET /organisations/${orgId}/members -> ${res.status()}`,
  ).toBeTruthy();
  return (await res.json()) as Array<{
    user_id: string;
    email: string;
    display_name: string;
    role: string;
  }>;
}

/**
 * `GET /organisations/{id}/user-lookup?email=…` — the email→user resolver behind the
 * members panel's add-by-email input. Returns the **raw** response so the caller can
 * assert on status: `200` (resolved) vs the opaque `404` the UI maps to
 * "No user with that email."
 */
export function userLookup(
  request: APIRequestContext,
  token: string,
  orgId: string,
  email: string,
) {
  return request.get(
    `${API}/organisations/${orgId}/user-lookup?email=${encodeURIComponent(email)}`,
    { headers: authHeaders(token) },
  );
}
