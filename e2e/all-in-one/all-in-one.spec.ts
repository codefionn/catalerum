import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

const OWNER = {
  email: "owner@all-in-one.test",
  displayName: "All-in-one Owner",
  password: "owner-password-1234",
};

const MEMBER = {
  email: "member@all-in-one.test",
  displayName: "Managed Member",
  initialPassword: "member-password-1234",
  resetPassword: "member-password-5678",
};

function bearer(token: string): Record<string, string> {
  return { Authorization: `Bearer ${token}` };
}

async function responseBody(response: Awaited<ReturnType<APIRequestContext["get"]>>) {
  return `${response.status()}: ${await response.text()}`;
}

async function openSettings(page: Page, tab: string): Promise<void> {
  await page.getByTitle("Settings").click();
  await expect(page.getByRole("heading", { name: "Settings", exact: true })).toBeVisible();
  await page.getByRole("button", { name: tab, exact: true }).click();
}

test("fresh all-in-one image supports setup, routing, users, and dynamic llmleaf topology", async ({
  page,
  request,
}) => {
  let ownerToken = "";

  await test.step("serve the frontend and anonymous API on one origin", async () => {
    const health = await request.get("/api/healthz");
    expect(health.status(), await responseBody(health)).toBe(200);

    const setup = await request.get("/api/auth/setup");
    expect(setup.status(), await responseBody(setup)).toBe(200);
    await expect(setup.json()).resolves.toEqual({ enabled: true, required: true });

    await page.goto("/");
    await expect(page.locator(".wb-login-card")).toBeVisible();
    await expect(page.getByRole("heading", { name: "Create the instance owner" })).toBeVisible();
    expect(new URL(page.url()).origin).toBe(
      new URL(process.env.CATALERUM_ALL_IN_ONE_URL ?? "http://127.0.0.1:18080").origin,
    );
  });

  await test.step("create the instance owner through the first-boot UI", async () => {
    const form = page.locator(".wb-login-form");
    await form.locator('input:not([type="email"]):not([type="password"])').fill(OWNER.displayName);
    await form.locator('input[type="email"]').fill(OWNER.email);
    await form.locator('input[type="password"]').fill(OWNER.password);
    await form.getByRole("button", { name: "Create owner" }).click();

    await expect(page.locator(".wb-header")).toBeVisible();
    await expect(page.locator(".wb-nav")).toBeVisible();
    ownerToken = await page.evaluate(() => localStorage.getItem("catalerum_token") ?? "");
    expect(ownerToken).not.toBe("");

    const setup = await request.get("/api/auth/setup");
    await expect(setup.json()).resolves.toEqual({ enabled: true, required: false });
    const repeated = await request.post("/api/auth/setup", {
      data: {
        email: "other@all-in-one.test",
        display_name: "Other Owner",
        password: "another-password-1234",
      },
    });
    expect(repeated.status()).toBe(409);
  });

  await test.step("report the single-node backing services", async () => {
    const status = await request.get("/api/status", { headers: bearer(ownerToken) });
    expect(status.status(), await responseBody(status)).toBe(200);
    const body = (await status.json()) as {
      healthy: boolean;
      mode: string;
      services: Array<{ name: string; detail: string; state: string }>;
    };
    expect(body.mode).toBe("multi_user");
    expect(body.healthy).toBe(true);
    expect(body.services).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ name: "SQLite", state: "up" }),
        expect.objectContaining({ name: "Qdrant (vectors)", state: "up" }),
        expect.objectContaining({ name: "Database graph", detail: "relational fallback", state: "up" }),
        expect.objectContaining({ name: "Coordination bus", detail: "in-process (single-node)", state: "up" }),
        expect.objectContaining({ name: "LLM gateway", state: "up" }),
      ]),
    );

    await openSettings(page, "Status");
    const settings = page.locator(".settings-content");
    await expect(settings.getByText("SQLite", { exact: true })).toBeVisible();
    await expect(settings.getByText("Qdrant (vectors)", { exact: true })).toBeVisible();
    await expect(settings.getByText("Database graph", { exact: true })).toBeVisible();
    await page.getByTitle("Close").click();
  });

  await test.step("create a user, reset its password, and log in", async () => {
    await openSettings(page, "Users");
    const section = page.locator(".settings-content");
    await expect(section.getByRole("heading", { name: "User management" })).toBeVisible();

    const createForm = section.locator(".settings-form").first();
    await createForm.locator('input[type="email"]').fill(MEMBER.email);
    await createForm.locator('input:not([type="email"]):not([type="password"])').fill(MEMBER.displayName);
    await createForm.locator('input[type="password"]').fill(MEMBER.initialPassword);
    await createForm.locator("select").selectOption("member");
    await createForm.getByRole("button", { name: "Create user" }).click();
    await expect(section.locator(".settings-form-notice")).toContainText(`Created ${MEMBER.email}.`);
    await expect(section.getByText(MEMBER.email, { exact: true })).toBeVisible();

    const resetForm = section.locator(".settings-form").nth(1);
    await resetForm.locator("select").selectOption({ label: `${MEMBER.displayName} (${MEMBER.email})` });
    await resetForm.getByPlaceholder("New password").fill(MEMBER.resetPassword);
    await resetForm.getByRole("button", { name: "Reset password" }).click();
    await expect(section.locator(".settings-form-notice")).toHaveText("Password updated.");

    const login = await request.post("/api/auth/password", {
      data: { email: MEMBER.email, password: MEMBER.resetPassword },
    });
    expect(login.status(), await responseBody(login)).toBe(200);
    const memberToken = ((await login.json()) as { token: string }).token;
    expect(memberToken).toBeTruthy();

    await page.getByTitle("Close").click();
    await page.goto(`/?token=${encodeURIComponent(memberToken)}`);
    await expect(page.locator(".wb-header")).toBeVisible();
    await page.getByTitle("Settings").click();
    await expect(page.getByRole("button", { name: "Users", exact: true })).toHaveCount(0);

    // Restore the owner for the remaining admin-only control-plane checks.
    await page.goto(`/?token=${encodeURIComponent(ownerToken)}`);
    await expect(page.locator(".wb-header")).toBeVisible();
  });

  await test.step("reconcile a provider and route through the llmleaf UI", async () => {
    await openSettings(page, "LLM providers");
    const section = page.locator(".settings-content");
    await expect(section.getByRole("heading", { name: "llmleaf control plane" })).toBeVisible();
    const form = section.locator(".settings-form");

    await section.getByRole("tab", { name: "Providers" }).click();
    await form.getByLabel("Provider name").fill("echo-managed");
    await form.getByLabel("Provider type").fill("echo");
    await form.getByRole("button", { name: "Add provider" }).click();
    await expect(section.getByText("echo-managed", { exact: true })).toBeVisible();

    await section.getByRole("tab", { name: "Routes" }).click();
    // Namespaced ids are the normal OpenRouter shape (`author/model`); keep the
    // slash here so the packaged test covers the topology path-tail contract.
    await form.getByLabel("Route model").fill("managed/echo");
    await form.getByLabel("Provider", { exact: true }).fill("echo-managed");
    await form.getByLabel("Upstream model").fill("managed-echo");
    await form.getByRole("button", { name: "Save route" }).click();
    await expect(section.getByText("managed/echo", { exact: true })).toBeVisible();

    await expect
      .poll(
        async () => {
          const models = await request.get("/api/llm-models?search=managed%2Fecho", {
            headers: bearer(ownerToken),
          });
          if (!models.ok()) return [];
          return (await models.json()) as unknown[];
        },
        { timeout: 20_000, message: "llmleaf should poll and expose the dynamic route" },
      )
      .toEqual(expect.arrayContaining([expect.objectContaining({ id: "managed/echo" })]));
  });
});
