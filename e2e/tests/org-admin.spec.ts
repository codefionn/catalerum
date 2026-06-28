import { test, expect, type Page } from "@playwright/test";
import {
  API,
  authHeaders,
  resolveSession,
  runSuffix,
  loginWith,
  createOrg,
  deleteOrg,
  listOrgs,
  createOrgWorkspace,
  archiveWorkspace,
  listOrgMembers,
  userLookup,
} from "./helpers";

// Org UX completions (SOUL §18, landed 2026-07-02):
//   (a) add-member-by-email: the members panel resolves a typed email through the
//       org-gated `user-lookup` route (a 404 → "No user with that email"). **The
//       members panel is `multi_user`-only** — `OrgPanel` gates it on
//       `(multi && is_admin)` (workspace.rs) and the seeded stack runs `single_user`
//       (config/catalerum.toml `mode = "single_user"`), so the panel + its email
//       input are *unreachable through the UI here*. We assert the single_user modal
//       shows the create-workspace UX but NOT the members panel, and cover the
//       lookup round-trip the panel would drive against the server directly.
//   (b) org deletion: an owner-only "Delete organisation" affordance on an org with
//       no visible workspaces (server re-checks: Owner-only, 409 if any workspace —
//       live *or* archived — remains). Deleting an empty org removes it from the
//       selector; deleting one that still holds a workspace 409s with the server's
//       verbatim reason surfaced.

const openOrgModal = async (page: Page) => {
  await page.getByRole("button", { name: "Organisations" }).click();
  await expect(
    page.getByRole("heading", { name: "Organisations" }),
  ).toBeVisible();
};
const closeOrgModal = (page: Page) =>
  page.locator(".settings-modal .settings-close").click();
// The manager's left rail lists the caller's orgs; click one to open its detail.
const selectOrg = (page: Page, label: string) =>
  page.locator(".org-rail-item", { hasText: label }).click();

test("member lookup round-trips server-side (members panel is multi_user-only)", async ({
  page,
  request,
}) => {
  const token = await resolveSession(request);
  test.skip(
    !token,
    "set CATALERUM_DEV_AUTHORIZATION_TOKEN (stable dev bearer) to run this spec",
  );
  const bearer = token as string;

  const sfx = runSuffix();
  const orgName = `E2E LookupOrg ${sfx}`;
  const orgSlug = `e2e-lookup-${sfx}`;
  let orgId = "";

  try {
    // A fresh org ⇒ the admin is its sole member (and its Owner, so `user-lookup`'s
    // org-admin gate passes). This also gives a self-contained org to probe.
    const org = await createOrg(request, bearer, { name: orgName, slug: orgSlug });
    orgId = org.id;

    // The seed is a single user; the org's one member is the admin. Grab their
    // canonical email + id so the lookup uses a real address (no hard-coding).
    const members = await listOrgMembers(request, bearer, orgId);
    expect(members.length).toBe(1);
    const admin = members[0];
    expect(admin.email).toBeTruthy();

    // --- The round-trip the (multi_user-only) email input would drive -----------
    // Exact-address hit → 200 resolving the same user id.
    const hit = await userLookup(request, bearer, orgId, admin.email);
    expect(hit.status(), `user-lookup(self) -> ${hit.status()}`).toBe(200);
    const resolved = (await hit.json()) as { user_id: string; email: string };
    expect(resolved.user_id).toBe(admin.user_id);

    // A bogus address → the opaque 404 the UI maps to "No user with that email".
    const miss = await userLookup(
      request,
      bearer,
      orgId,
      `nobody-${sfx}@example.invalid`,
    );
    expect(miss.status(), `user-lookup(bogus) -> ${miss.status()}`).toBe(404);

    // --- What the single_user org modal actually exposes ------------------------
    await loginWith(page, bearer);
    await openOrgModal(page);
    await selectOrg(page, orgName);

    // The create-workspace UX is present in single_user…
    await expect(
      page.locator("section.settings-section").filter({ hasText: "New workspace" }),
    ).toBeVisible();
    // …but the members panel (and its add-member-by-email input) is NOT — it is
    // `multi_user`-gated, so it is genuinely unreachable through the UI here.
    await expect(
      page.getByText("Add member (by email or user id)"),
    ).toHaveCount(0);
    await closeOrgModal(page);
  } finally {
    if (orgId) await deleteOrg(request, bearer, orgId); // empty ⇒ deletes cleanly
  }
});

test("an owner can delete an empty organisation from the modal", async ({
  page,
  request,
}) => {
  const token = await resolveSession(request);
  test.skip(
    !token,
    "set CATALERUM_DEV_AUTHORIZATION_TOKEN (stable dev bearer) to run this spec",
  );
  const bearer = token as string;

  const sfx = runSuffix();
  const orgName = `E2E DeleteOrg ${sfx}`;
  const orgSlug = `e2e-delete-${sfx}`;
  let orgId = "";

  try {
    const org = await createOrg(request, bearer, { name: orgName, slug: orgSlug });
    orgId = org.id;

    await loginWith(page, bearer);
    await openOrgModal(page);
    await selectOrg(page, orgName);

    // Owner + no workspaces ⇒ the danger-zone delete affordance is offered.
    await expect(page.getByText("Danger zone")).toBeVisible();
    const del = page.getByRole("button", { name: "Delete organisation" });
    await expect(del).toBeVisible();

    // Confirm the native dialog, then delete.
    page.once("dialog", (d) => d.accept());
    await del.click();

    // Gone from the manager's rail (the reload drops it), and gone server-side.
    await expect(
      page.locator(".org-rail-item", { hasText: orgName }),
    ).toHaveCount(0);
    await expect
      .poll(async () =>
        (await listOrgs(request, bearer)).some((o) => o.slug === orgSlug),
      )
      .toBe(false);
    orgId = ""; // deleted — skip cleanup
  } finally {
    if (orgId) await deleteOrg(request, bearer, orgId);
  }
});

test("an org holding an archived workspace disables the delete button (archived count) and the API 409s", async ({
  page,
  request,
}) => {
  const token = await resolveSession(request);
  test.skip(
    !token,
    "set CATALERUM_DEV_AUTHORIZATION_TOKEN (stable dev bearer) to run this spec",
  );
  const bearer = token as string;

  const sfx = runSuffix();
  // Run-scoped + self-describing: this org CANNOT be deleted afterwards (its archived
  // workspace still blocks it — a known design gap: archive is the only removal path,
  // so an org that ever held a workspace lingers), so it is named to stay identifiable.
  const orgName = `E2E Undeletable ${sfx}`;
  const orgSlug = `e2e-undeletable-${sfx}`;
  let orgId = "";

  try {
    const org = await createOrg(request, bearer, { name: orgName, slug: orgSlug });
    orgId = org.id;
    const ws = await createOrgWorkspace(request, bearer, orgId, {
      name: `WS ${sfx}`,
      slug: `e2e-uws-${sfx}`,
    });
    // Archiving hides the workspace from the caller's org→workspace view, but the
    // org-admin **shell** listing the modal fetches still shows it, and the server
    // counts archived shells against deletion.
    await archiveWorkspace(request, bearer, orgId, ws.id);

    // --- Server stays the authority: a direct DELETE 409s verbatim ----------------
    // (the button below is now disabled, so we cover the 409 at the REST level).
    const del409 = await request.delete(`${API}/organisations/${orgId}`, {
      headers: authHeaders(bearer),
    });
    expect(
      del409.status(),
      `DELETE /organisations/${orgId} -> ${del409.status()}`,
    ).toBe(409);
    expect(await del409.text()).toContain("still has workspaces");

    // --- UI: the affordance is offered but disabled, with the explanatory tooltip --
    await loginWith(page, bearer);
    await openOrgModal(page);
    await selectOrg(page, orgName);

    const del = page.getByRole("button", { name: "Delete organisation" });
    await expect(del).toBeVisible();
    // The archived shell disables the button (no round-trip that would 409)…
    await expect(del).toBeDisabled();
    // …and its tooltip explains why (archived workspaces count against deletion).
    await expect(del).toHaveAttribute("title", /still has workspaces/);

    // And it really wasn't deleted.
    expect(
      (await listOrgs(request, bearer)).some((o) => o.slug === orgSlug),
    ).toBe(true);
    await closeOrgModal(page);
  } finally {
    // Known gap: an org with an archived workspace can't be removed (no hard delete).
    // Best-effort attempt (409, ignored); the run-scoped org lingers by design.
    if (orgId) await deleteOrg(request, bearer, orgId);
  }
});
