import { test, expect } from "@playwright/test";
import {
  resolveSession,
  runSuffix,
  gotoPanel,
  createAutomation,
  deleteAutomation,
} from "./helpers";

// Automations web affordances (SOUL §11/§12, landed 2026-07-02):
//   - a "Collect now" button on a saved collect-headed automation → "Collect started."
//   - a "Fire signal" block on a `trigger`-headed automation → "Fired — N automation(s) matched."
//
// **Automation-creation choice: REST.** Building a collect-/trigger-headed
// automation through the visual builder is heavy and brittle for e2e, so we create
// the two fixtures via `POST /automations` with the admin bearer (the choice the
// task explicitly allows) and drive only the *affordances* through the UI. A
// minimal `{"kind":"summarize"}` action satisfies the server's spec validator (its
// own route test uses the same shape); the collect fixture stays `enabled:false` so
// the scheduler never polls its placeholder connection, while the trigger fixture is
// enabled so a fire actually matches it.
//
// Note: the web "Fire"/"Collect now" buttons post to `/{triggers,automations}/{name}`
// using the *automation's own name*, so the trigger fixture's signal name is set
// equal to its automation name to get a `matched: 1` result.

test("collect + trigger automation affordances drive their endpoints", async ({
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
  const collectName = `e2e-collect-${sfx}`;
  const signalName = `e2e-signal-${sfx}`;

  try {
    // --- Fixtures via REST ---------------------------------------------------
    await createAutomation(request, bearer, {
      name: collectName,
      enabled: false, // no scheduler polling of the placeholder connection
      triggers: [{ kind: "collect_calendar", connection: `e2e-conn-${sfx}` }],
      actions: [{ kind: "summarize" }],
    });
    await createAutomation(request, bearer, {
      name: signalName,
      enabled: true, // must be enabled for a fire to match it
      triggers: [{ kind: "trigger", name: signalName }],
      actions: [{ kind: "summarize" }],
    });

    await gotoPanel(page, bearer, "/app/automations");

    // --- Collect now on the collect-headed automation ------------------------
    await page.locator("button.pane-item", { hasText: collectName }).click();
    const collectNow = page.getByRole("button", { name: "Collect now" });
    await expect(collectNow).toBeVisible();
    await collectNow.click();
    await expect(page.getByText("Collect started.")).toBeVisible();

    // --- Fire signal on the trigger-headed automation ------------------------
    await page.locator("button.pane-item", { hasText: signalName }).click();
    await expect(page.getByText("Fire signal")).toBeVisible();

    const fireBlock = page.locator(".auto-fire");
    await fireBlock.locator("textarea").fill('{"e2e":1}');
    await fireBlock.getByRole("button", { name: "Fire", exact: true }).click();

    // Unique signal name ⇒ exactly this automation matches ⇒ "1 automation matched".
    await expect(page.getByText(/Fired.*1 automation matched\./)).toBeVisible();
  } finally {
    await deleteAutomation(request, bearer, collectName);
    await deleteAutomation(request, bearer, signalName);
  }
});
