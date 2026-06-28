import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import {
  API,
  authHeaders,
  resolveSession,
  runSuffix,
  gotoPanel,
  createAutomation,
  deleteAutomation,
} from "./helpers";

// Collect cadence builder field (SOUL §11/§29, landed 2026-07-02): a collect source
// trigger carries an optional `every` poll cadence (bare minutes, a duration string
// like `5m`/`1h30m`, or `{"seconds":N}`), clamped `[60s, 1 week]` server-side. The
// visual builder exposes it as a free-text "Poll cadence" row with a clamp hint and a
// **soft** shape warning that never blocks saving (the server re-parses + clamps).
//
// The field renders in the FlowEditor's node config panel, so the fixture is a
// **graph** automation: we POST a `spec.graph` with a `collect_calendar` trigger node
// (the server compiles its dispatch triggers from the graph), which the panel opens
// in Visual mode. `enabled:false` keeps the scheduler from polling the placeholder
// connection. We drive the field, save, and confirm persistence both in the reopened
// builder and via the stored `spec.graph…trigger.every`.

const CADENCE_PLACEHOLDER = 'e.g. 5m, 1h30m, or {"seconds":90}';

/** Read the persisted cadence off the automation's stored graph trigger. */
async function readEvery(
  request: APIRequestContext,
  bearer: string,
  name: string,
): Promise<unknown> {
  const res = await request.get(`${API}/automations/${encodeURIComponent(name)}`, {
    headers: authHeaders(bearer),
  });
  expect(res.ok(), `GET /automations/${name} -> ${res.status()}`).toBeTruthy();
  const a = (await res.json()) as {
    spec?: { graph?: { nodes?: Array<Record<string, any>> } };
  };
  const nodes = a.spec?.graph?.nodes ?? [];
  const trigger = nodes.find((n) => n.kind === "trigger");
  return trigger?.trigger?.every;
}

/** Open the automation in the Visual builder and select its collect trigger node. */
async function openTriggerConfig(page: Page, name: string): Promise<void> {
  await page.locator("button.pane-item", { hasText: name }).click();
  // The builder opens in Visual mode (the stored spec.graph round-trips). Frame the
  // nodes so the trigger is on-screen, then select it → the config panel appears.
  await expect(page.locator("svg.flow-canvas")).toBeVisible();
  await page.getByRole("button", { name: "Fit" }).click();
  await page.locator("g.flow-node-trigger .flow-node-box").click();
  await expect(
    page.locator(".flow-config-title", { hasText: "trigger · t1" }),
  ).toBeVisible();
}

test("the collect cadence field renders, persists, and warns without blocking save", async ({
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
  const name = `e2e-cadence-${sfx}`;
  const graphSpec = {
    graph: {
      nodes: [
        {
          id: "t1",
          kind: "trigger",
          trigger: { kind: "collect_calendar", connection: `e2e-cad-conn-${sfx}` },
          position: { x: 160, y: 140 },
        },
        {
          id: "a1",
          kind: "action",
          action: { kind: "summarize" },
          position: { x: 460, y: 140 },
        },
      ],
      edges: [{ from: "t1", to: "a1", from_port: "", to_port: "" }],
    },
  };

  const saveBtn = () => page.locator('form.auto-form button[type="submit"]');
  const cadence = () => page.getByPlaceholder(CADENCE_PLACEHOLDER);

  try {
    // Fixture: a graph automation the builder opens in Visual mode.
    await createAutomation(request, bearer, {
      name,
      enabled: false, // never poll the placeholder connection
      spec: graphSpec,
    });

    // --- The field renders with its clamp hint ------------------------------
    await gotoPanel(page, bearer, "/app/automations");
    await openTriggerConfig(page, name);
    await expect(cadence()).toBeVisible();
    await expect(
      page.locator(".flow-cfg-hint", { hasText: "Clamped to 60s" }),
    ).toBeVisible();

    // --- Type a valid cadence + save ----------------------------------------
    await cadence().fill("5m");
    await saveBtn().click();
    await expect(page.locator(".auto-form-error")).toHaveCount(0);
    // Persisted onto the stored graph trigger.
    await expect.poll(() => readEvery(request, bearer, name)).toBe("5m");

    // --- Re-open the builder → the cadence persisted ------------------------
    await gotoPanel(page, bearer, "/app/automations");
    await openTriggerConfig(page, name);
    await expect(cadence()).toHaveValue("5m");

    // --- Garbage warns softly but does NOT block saving ---------------------
    await cadence().fill("5x");
    await expect(
      page.locator(".flow-cfg-warn", { hasText: "Unrecognized cadence shape" }),
    ).toBeVisible();
    // The save button is still enabled — the warning is advisory only.
    await expect(saveBtn()).toBeEnabled();
    await saveBtn().click();
    await expect(page.locator(".auto-form-error")).toHaveCount(0);
    // The unrecognized value was still saved verbatim (proving save wasn't blocked).
    await expect.poll(() => readEvery(request, bearer, name)).toBe("5x");
  } finally {
    await deleteAutomation(request, bearer, name);
  }
});
