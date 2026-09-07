#!/usr/bin/env node
// SPDX-License-Identifier: GPL-3.0-or-later
// Serve the browser page with its release bundle, then pass its loopback URL.
// Requires Playwright. PLAYWRIGHT_MODULE and CHROME_PATH may select local tools.
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const url = new URL(process.argv[2] ?? 'http://127.0.0.1:8000/');
assert.ok(['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname), 'Use a local test page');
const output = resolve(process.argv[3] ?? '/tmp/copperline-web-netplay-browser');
await mkdir(output, { recursive: true });
const module = process.env.PLAYWRIGHT_MODULE;
const { chromium } = await import(module ? pathToFileURL(resolve(module)).href : 'playwright');
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH,
  args: ['--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling',
    '--disable-renderer-backgrounding', '--disable-backgrounding-occluded-windows'] });
const errors = [];
try {
  const pages = [];
  for (let player = 0; player < 2; player++) {
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 },
      hasTouch: player === 0, serviceWorkers: 'block' });
    // Tests exchange LAN candidates only and do not contact analytics or STUN.
    await context.route('**/*', route => {
      const target = new URL(route.request().url());
      return target.origin === url.origin ? route.continue() : route.abort();
    });
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(url.href);
    await page.locator('#boot:enabled').waitFor({ timeout: 30000 });
    if (await page.locator('#netplay-open').count()) await page.locator('#netplay-open').click();
    else await page.locator('#netplay-panel > summary').click();
    await page.locator('#netplay-advanced').evaluate(panel => { panel.open = true; });
    await page.locator('#netplay-stun').fill('');
    pages.push(page);
  }
  const [host, guest] = pages;
  // Hold a local boot at its audio await, enter/cancel netplay, then let the
  // obsolete boot complete. It must not resurrect a local machine after setup.
  await host.evaluate(async () => {
    const probe = new AudioContext();
    const proto = Object.getPrototypeOf(probe.audioWorklet);
    const addModule = proto.addModule;
    let first = true;
    proto.addModule = async function (...args) {
      if (!first) return addModule.apply(this, args);
      first = false;
      window.__testAudioWaiting = true;
      await new Promise(resolve => { window.__releaseTestAudio = resolve; });
      try { return await addModule.apply(this, args); }
      finally { window.__testAudioDone = true; proto.addModule = addModule; }
    };
    await probe.close();
  });
  await host.locator('#boot').click();
  await host.waitForFunction(() => window.__testAudioWaiting);
  await host.locator('#netplay-host').click();
  await host.locator('#netplay-disconnect').click();
  await host.evaluate(() => window.__releaseTestAudio());
  await host.waitForFunction(() => window.__testAudioDone);
  await host.waitForFunction(() => !window.__emu && !document.querySelector('#boot').disabled);
  await host.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  assert.equal(await host.evaluate(() => window.__emu == null), true, 'cancelled local boot must stay stopped');
  async function connect() {
    await host.locator('#netplay-host').click();
    await host.waitForFunction(() => document.querySelector('#netplay-local').value.startsWith('CLNP1.'));
    await guest.locator('#netplay-remote').fill(await host.locator('#netplay-local').inputValue());
    await guest.locator('#netplay-join').click();
    await guest.waitForFunction(() => document.querySelector('#netplay-local').value.startsWith('CLNP1.'));
    await host.locator('#netplay-remote').fill(await guest.locator('#netplay-local').inputValue());
    await host.locator('#netplay-accept').click();
  }
  // Cancel a gathered offer and prove the setup can be used again.
  await host.locator('#netplay-host').click();
  await host.waitForFunction(() => document.querySelector('#netplay-local').value.startsWith('CLNP1.'));
  await host.locator('#netplay-disconnect').click();
  assert.equal(await host.locator('#boot').isEnabled(), true);
  await connect();
  await Promise.all(pages.map(page => page.waitForFunction(() => window.__emu?.netplay_status()[6] >= 120,
    null, { timeout: 90000 })));
  await host.bringToFront();
  await host.evaluate(() => {
    const emu = window.__emu;
    const take = emu.netplay_take_packet.bind(emu);
    const [protocol, , headerBytes, inputBytes] = emu.constructor.netplay_packet_layout();
    if (protocol !== 2 || headerBytes !== 111 || inputBytes !== 31) throw new Error('Packet decoder layout changed');
    window.__testKeyFrames = [];
    emu.netplay_take_packet = () => {
      const packet = take();
      // Protocol v2: 111-byte header, then 31-byte input records. Check the
      // emitted held-key states, not merely the UI's keydown/up callbacks.
      const view = new DataView(packet.buffer, packet.byteOffset, packet.byteLength);
      for (let offset = headerBytes; offset + inputBytes <= packet.length; offset += inputBytes) {
        window.__testKeyFrames.push([Number(view.getBigUint64(offset, true)),
          !!(packet[offset + 10 + (0x20 >> 3)] & (1 << (0x20 & 7)))]);
      }
      return packet;
    };
  });
  await host.locator('#devkeyboard').click();
  await host.getByLabel('Type into the Amiga').evaluate(field =>
    field.dispatchEvent(new InputEvent('beforeinput', { inputType: 'insertText', data: 'a', bubbles: true, cancelable: true })));
  await host.waitForFunction(() => {
    const frames = window.__testKeyFrames;
    const down = frames.find(([frame, pressed]) => pressed);
    return down && frames.some(([frame, pressed]) => !pressed && frame > down[0]);
  }).catch(async error => {
    console.error(await host.evaluate(() => ({ frames: window.__testKeyFrames,
      focused: document.hasFocus(), active: document.activeElement?.getAttribute('aria-label'),
      status: document.querySelector('#netplay-status').textContent })));
    throw error;
  });
  await host.locator('#devkeyboard').click();
  for (const [player, page] of pages.entries()) {
    for (const id of ['boot', 'machine', 'video', 'reset', 'pause', 'df0', 'df1', 'floppy-speed', 'floppy-sounds']) {
      assert.equal(await page.locator(`#${id}`).isDisabled(), true, `${id} must stay locked`);
    }
    const status = await page.evaluate(() => [...window.__emu.netplay_status()]);
    console.log(`Browser player ${player + 1}: ${JSON.stringify(status)}`);
  }
  await host.locator('#netplay-panel').screenshot({ path: `${output}/netplay-desktop.png` });
  await host.locator('#netplay-disconnect').click();
  await Promise.all(pages.map(page => page.waitForFunction(() => !window.__emu && !document.querySelector('#netplay-host').disabled)));
  await host.locator('#netplay-controller').selectOption('mouse');
  await connect();
  await Promise.all(pages.map(page => page.waitForFunction(() => window.__emu?.netplay_status()[6] >= 60,
    null, { timeout: 60000 })));
  for (const page of pages) {
    await page.bringToFront();
    assert.equal(await page.locator('#joy').textContent(), 'Joystick: off');
    await page.evaluate(() => {
      const emu = window.__emu;
      const take = emu.netplay_take_packet.bind(emu);
      window.__testMouseFrames = [];
      emu.netplay_take_packet = () => {
        const packet = take();
        const view = new DataView(packet.buffer, packet.byteOffset, packet.byteLength);
        for (let offset = 111; offset + 31 <= packet.length; offset += 31) {
          window.__testMouseFrames.push([Number(view.getBigUint64(offset, true)),
            view.getInt16(offset + 26, true), view.getInt16(offset + 28, true), packet[offset + 30]]);
        }
        return packet;
      };
      const canvas = document.querySelector('canvas');
      canvas.dispatchEvent(new MouseEvent('mousemove', { clientX: 40, clientY: 40, bubbles: true }));
      canvas.dispatchEvent(new MouseEvent('mousemove', { clientX: 46, clientY: 43, bubbles: true }));
      // Right and middle clicks exercise the DOM-to-Amiga button mapping
      // without requesting pointer lock from a synthetic gesture.
      canvas.dispatchEvent(new MouseEvent('mousedown', { button: 2, bubbles: true }));
      canvas.dispatchEvent(new MouseEvent('mousedown', { button: 1, bubbles: true }));
    });
    await page.waitForFunction(() => window.__testMouseFrames.some(([, dx, dy, buttons]) => dx > 0 && dy > 0 && buttons === 6));
    await page.evaluate(() => {
      window.dispatchEvent(new MouseEvent('mouseup', { button: 2 }));
      window.dispatchEvent(new MouseEvent('mouseup', { button: 1 }));
    });
    await page.waitForFunction(() => {
      const frames = window.__testMouseFrames;
      const held = frames.find(([, , , buttons]) => buttons === 6);
      return held && frames.some(([frame, dx, dy, buttons]) => frame > held[0] && dx === 0 && dy === 0 && buttons === 0);
    });
  }
  await guest.locator('#netplay-disconnect').click();
  await Promise.all(pages.map(page => page.locator('#netplay-host:enabled').waitFor()));
  // Manual signaling also transfers the host setup over the peer connection.
  await guest.locator('#video').selectOption('NTSC');
  await connect();
  await Promise.all(pages.map(page => page.waitForFunction(() => window.__emu?.netplay_status()[6] >= 60,
    null, { timeout: 60000 })));
  assert.equal(await guest.locator('#video').inputValue(), 'PAL');
  await guest.locator('#netplay-disconnect').click();
  await Promise.all(pages.map(page => page.locator('#netplay-host:enabled').waitFor()));
  assert.equal(await guest.locator('#video').inputValue(), 'NTSC');
  await host.setViewportSize({ width: 390, height: 844 });
  await host.locator('#sidebar-toggle').click();
  await host.locator('#netplay-panel').screenshot({ path: `${output}/netplay-mobile.png` });
  assert.deepEqual(errors, []);
  console.log('Host/Join, cancellation, restart, two-mouse input, control locking, host setup transfer and browser rendering passed');
} finally { await browser.close(); }
