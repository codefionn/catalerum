import { test, expect } from "@playwright/test";
import { API, resolveSession, runSuffix, gotoPanel } from "./helpers";

// Dormant-connection warning (SOUL §12/§28/§29, landed 2026-07-02): a configured
// connection that no enabled Collect automation ingests from is flagged in the UI
// as idle, closing the "I added my calendar but see no events" trap.
//
// We create a *local* (`.ics`) calendar connection through the calendar panel's
// "Connect calendar" form — a local source needs no external server, and the
// seeded fixtures ship no collecting automation for it — so it is dormant on
// creation and the sidebar's "Sources" list should show the idle notice under it.
// (The email side has no standalone connections panel in the web UI, so the
// visible warning lives only in the calendar Sources list.)

test("a calendar source with no Collect automation is flagged idle", async ({
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
  const connName = `E2E Dormant ${sfx}`;

  try {
    await gotoPanel(page, bearer, "/app/calendar");

    // Open the "Connect calendar" form (provider defaults to Local).
    await page.getByRole("button", { name: "Connect calendar" }).click();
    await page.getByPlaceholder("Work calendar").fill(connName);
    await page.getByPlaceholder("/srv/calendars").fill(`/tmp/e2e-cal-${sfx}`);
    await page.getByRole("button", { name: "Connect & sync" }).click();

    // The new source appears in the sidebar "Sources" list. With no Collect
    // automation referencing it, it is dormant → the idle notice shows under it.
    const source = page.locator(".cal-source", { hasText: connName });
    await expect(source).toBeVisible();
    await expect(source.locator(".cal-source-idle")).toContainText(
      "nothing collects from this source yet",
    );
  } finally {
    // Best-effort cleanup: find the connection by name and delete it so the
    // shared dev stack stays clean for the other agents.
    const res = await request
      .get(`${API}/connections`, {
        headers: { Authorization: `Bearer ${bearer}` },
      })
      .catch(() => null);
    if (res && res.ok()) {
      const conns = (await res.json()) as Array<{ id: string; name: string }>;
      for (const c of conns.filter((c) => c.name === connName)) {
        await request
          .delete(`${API}/connections/${c.id}`, {
            headers: { Authorization: `Bearer ${bearer}` },
          })
          .catch(() => undefined);
      }
    }
  }
});
