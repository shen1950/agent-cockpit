const { test } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const { chromium } = require('playwright');

test('dragging the image moves the window without starting an image drag or opening the panel', async () => {
  const browser = await chromium.launch({channel: 'msedge', headless: true});
  try {
    const page = await browser.newPage({viewport: {width: 450, height: 560}});
    await page.addInitScript(() => {
      window.calls = [];
      window.dragEvents = 0;
      document.addEventListener('dragstart', () => { window.dragEvents++; });
      window.__TAURI__ = {
        core: { invoke: async (command, args) => {
          if (command === 'get_state') return {alerts: []};
          window.calls.push(args.op);
          if (args.op === 'layout') return {x: 300, y: 200, edge: ''};
          if (args.op === 'feed') return {events: [], recent: []};
          return {};
        }},
        event: {listen: async () => {}}
      };
    });
    await page.goto(pathToFileURL(path.resolve(__dirname, '../ui/companion.html')).href + '?kind=orb');
    await page.waitForFunction(() => window.calls.includes('layout'));
    const image = page.locator('#handle img');
    assert.equal(await image.getAttribute('draggable'), 'false');
    const box = await image.boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width / 2 + 35, box.y + box.height / 2 + 20, {steps: 8});
    await page.mouse.up();
    await page.waitForTimeout(150);
    assert.equal(await page.evaluate(() => window.dragEvents), 0);
    assert.equal(await page.evaluate(() => window.calls.filter(x => x === 'drag').length), 1);
    assert.equal(await page.evaluate(() => window.calls.includes('show')), false);
    await page.locator('#handle').click();
    assert.equal(await page.evaluate(() => window.calls.includes('show')), true);
  } finally {
    await browser.close();
  }
});
