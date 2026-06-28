// Render an HTML file to a PNG image with headless Chromium (Playwright), so the
// engine's Mermaid SVG + LaTeX MathML output can be inspected as a real image.
//
//   node crates/catalerum-markdown/examples/render.mjs <input.html> <output.png>
//
// Chromium renders SVG and MathML natively — no client-side JS, KaTeX or
// mermaid.js involved. Requires Playwright's chromium (already used by the e2e
// suite): `npx playwright install chromium` if missing.

import { chromium } from 'playwright';
import path from 'node:path';

const htmlPath = path.resolve(process.argv[2] || 'gallery.html');
const outPath = path.resolve(process.argv[3] || 'gallery.png');

const browser = await chromium.launch();
const page = await browser.newPage({
  viewport: { width: 920, height: 1400 },
  deviceScaleFactor: 2,
});
await page.goto('file://' + htmlPath, { waitUntil: 'networkidle' });
// Give fonts/layout a moment to settle.
await page.waitForTimeout(250);
await page.screenshot({ path: outPath, fullPage: true });
await browser.close();
console.log('wrote', outPath);
