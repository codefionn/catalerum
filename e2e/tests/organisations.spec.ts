import { test, expect, type Locator } from "@playwright/test";
import { API, resolveSession, runSuffix, loginWith } from "./helpers";

// Organisations web half + org UX + soft-archive (SOUL §12/§18, landed 2026-07-02):
//   - a header "Organisations" button opens a mode-aware management modal
//   - create an organisation, then a workspace inside it
//   - the header switcher groups workspaces under their organisation (<optgroup>s)
//     and can switch into a new workspace
//   - archive a workspace (reversible soft-archive, behind a confirm) → it leaves
//     the switcher and appears in the modal's "Archived" subsection
//   - restore it → it returns to the switcher
//
// This runs in the seeded `single_user` mode: member/role/SSO chrome is hidden, so
// we assert what IS visible per mode — the create flows and the archive/restore
// shell administration (which is not mode-restricted; the server gates it).
//
// The manager is a two-pane modal: an organisation rail on the left (`.org-rail`,
// one `.org-rail-item` per org + a "+ New organisation" entry) and the selected
// org's detail on the right.

// The create-org and create-workspace forms both use placeholder="Name"/"slug",
// so they are scoped by their enclosing section.
function orgSection(page: import("@playwright/test").Page): Locator {
  return page
    .locator("section.settings-section")
    .filter({ has: page.getByRole("heading", { name: "New organisation" }) });
}
function wsSection(page: import("@playwright/test").Page): Locator {
  // The selected org's workspaces section — the only section that carries
  // "New workspace"; it also hosts the workspace rows (archive/restore).
  return page
    .locator("section.settings-section")
    .filter({ hasText: "New workspace" });
}

test("create org + workspace, switch, then archive + restore", async ({
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
  const orgName = `E2E Org ${sfx}`;
  const orgSlug = `e2e-org-${sfx}`;
  const wsName = `E2E WS ${sfx}`;
  const wsSlug = `e2e-ws-${sfx}`;

  const switcher = page.locator("select.wb-workspace");
  const openModal = async () => {
    await page.getByRole("button", { name: "Organisations" }).click();
    await expect(
      page.getByRole("heading", { name: "Organisations" }),
    ).toBeVisible();
  };
  const closeModal = () => page.locator(".settings-modal .settings-close").click();
  // Pick the org in the rail (its detail pane opens on the right).
  const selectE2eOrg = () =>
    page.locator(".org-rail-item", { hasText: orgName }).click();

  try {
    await loginWith(page, bearer);

    // --- Create a new organisation ------------------------------------------
    await openModal();
    // The create flow lives behind the rail's "+ New organisation" entry.
    await page.getByRole("button", { name: "+ New organisation" }).click();
    const org = orgSection(page);
    await org.getByPlaceholder("Name").fill(orgName);
    await org.getByPlaceholder("slug").fill(orgSlug);
    await org.getByRole("button", { name: "Create" }).click();
    await expect(page.getByText(`Created organisation “${orgName}”.`)).toBeVisible();

    // The new org appears in the rail; open its detail pane.
    await expect(page.locator(".org-rail-item", { hasText: orgName })).toBeVisible();
    await selectE2eOrg();

    // --- Create a workspace inside it ---------------------------------------
    // NB: the create-workspace success *notice* is transient — the panel that
    // hosts it (OrgPanel) remounts when the create's `reload` refetches the org
    // list, so we don't assert on the notice. The durable, task-relevant outcome
    // is the new workspace appearing in the switcher, asserted just below.
    const ws = wsSection(page);
    await ws.getByPlaceholder("Name").fill(wsName);
    await ws.getByPlaceholder("slug").fill(wsSlug);
    await ws.getByRole("button", { name: "Create" }).click();
    await closeModal();

    // --- Switcher now groups by organisation (optgroups) --------------------
    await expect(switcher).toBeVisible();
    await expect(
      page.locator(`select.wb-workspace optgroup[label="${orgName}"]`),
    ).toHaveCount(1);
    await expect(switcher.getByRole("option", { name: wsName })).toHaveCount(1);

    // --- Switch into the new workspace, then back to Default ----------------
    await switcher.selectOption({ label: wsName }).catch(() => undefined);
    await expect(
      page.locator("select.wb-workspace option:checked"),
    ).toHaveText(wsName);

    await page
      .locator("select.wb-workspace")
      .selectOption({ label: "Default" })
      .catch(() => undefined);
    await expect(
      page.locator("select.wb-workspace option:checked"),
    ).toHaveText("Default");

    // --- Archive the workspace (reversible, behind a confirm) ---------------
    await openModal();
    await selectE2eOrg();
    page.once("dialog", (d) => d.accept()); // confirm: "This is reversible…"
    await wsSection(page)
      .locator("li.org-ws-row", { hasText: wsName })
      .getByRole("button", { name: "Archive" })
      .click();

    // It moves into the "Archived" subsection (flagged, with a Restore action).
    const archivedRow = wsSection(page).locator("li.org-ws-row", {
      hasText: wsName,
    });
    await expect(archivedRow.locator(".org-badge-archived")).toHaveText(
      "archived",
    );
    await expect(archivedRow.getByRole("button", { name: "Restore" })).toBeVisible();
    await closeModal();

    // Gone from the switcher (only Default remains ⇒ the select is hidden).
    await expect(switcher.getByRole("option", { name: wsName })).toHaveCount(0);

    // --- Restore it → it returns to the live list and the switcher ----------
    await openModal();
    await selectE2eOrg();
    await wsSection(page)
      .locator("li.org-ws-row", { hasText: wsName })
      .getByRole("button", { name: "Restore" })
      .click();
    await expect(
      wsSection(page)
        .locator("li.org-ws-row", { hasText: wsName })
        .getByRole("button", { name: "Archive" }),
    ).toBeVisible();
    await closeModal();

    await expect(switcher).toBeVisible();
    await expect(
      page.locator(`select.wb-workspace optgroup[label="${orgName}"]`),
    ).toHaveCount(1);
    await expect(switcher.getByRole("option", { name: wsName })).toHaveCount(1);
  } finally {
    // Good-citizen cleanup on the shared dev stack: re-archive the workspace via
    // REST so the shared admin's switcher returns to its single-workspace state.
    // (There is no delete-org API, so the empty org itself lingers harmlessly.)
    const res = await request
      .get(`${API}/organisations`, {
        headers: { Authorization: `Bearer ${bearer}` },
      })
      .catch(() => null);
    if (res && res.ok()) {
      const orgs = (await res.json()) as Array<{
        id: string;
        slug: string;
        workspaces: Array<{ id: string; slug: string }>;
      }>;
      const org = orgs.find((o) => o.slug === orgSlug);
      const wsId = org?.workspaces.find((w) => w.slug === wsSlug)?.id;
      if (org && wsId) {
        await request
          .delete(`${API}/organisations/${org.id}/workspaces/${wsId}`, {
            headers: { Authorization: `Bearer ${bearer}` },
          })
          .catch(() => undefined);
      }
    }
  }
});
