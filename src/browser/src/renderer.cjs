// Launched under OS network denial, inherited by Chromium and all descendants.
// No request may continue: public resources come only from the Rust broker.
// Playwright normally detaches Chromium into a new process group. Keep every child
// in the Rust-owned group so cancellation/timeout kills the complete process tree.
const childProcess = require('node:child_process');
const spawn = childProcess.spawn;
childProcess.spawn = (file, args, options) => spawn(file, args, { ...options, detached: false });
const { chromium } = require('playwright');
const readline = require('node:readline');
const pending = new Map();
let sequence = 0;
const send = (message) => process.stdout.write(JSON.stringify(message) + '\n');
readline.createInterface({ input: process.stdin }).on('line', (line) => {
  const reply = JSON.parse(line);
  const resolve = pending.get(reply.id);
  pending.delete(reply.id);
  resolve?.(reply);
});
async function main() {
  const browser = await chromium.launch({ headless: true, args: ['--disable-background-networking', '--disable-quic', '--disable-extensions', '--disable-sync', '--host-resolver-rules=MAP * ~NOTFOUND', '--force-webrtc-ip-handling-policy=disable_non_proxied_udp'] });
  const context = await browser.newContext({ acceptDownloads: false, serviceWorkers: 'block', permissions: [] });
  context.on('page', (page) => { page.on('dialog', (dialog) => dialog.dismiss()); page.on('download', (download) => download.cancel()); });
  await context.routeWebSocket('**/*', (socket) => socket.close());
  // Workers are not needed for document rendering and can create unrouteable traffic.
  await context.addInitScript(() => { globalThis.Worker = class { constructor() { throw new Error('Workers disabled'); } }; globalThis.SharedWorker = globalThis.Worker; });
  const sessions = new WeakMap();
  function intercept(target) {
    if (sessions.has(target)) return sessions.get(target);
    const ready = (async () => {
      const session = await context.newCDPSession(target);
      const chains = new Map();
      session.on('Fetch.authRequired', ({ requestId }) => {
        void session.send('Fetch.continueWithAuth', { requestId, authChallengeResponse: { response: 'CancelAuth' } }).catch(() => {});
      });
      session.on('Fetch.requestPaused', async ({ requestId, request, resourceType, redirectedRequestId }) => {
        const abort = () => session.send('Fetch.failRequest', { requestId, errorReason: 'BlockedByClient' }).catch(() => {});
        try {
          const id = ++sequence;
          if (id > 100) { send({ type: 'failure', status: 509 }); return abort(); }
          const previous = chains.get(redirectedRequestId) || [];
          if (previous.includes(request.url)) { send({ type: 'failure', status: 508 }); return abort(); }
          if (previous.length > 8) { send({ type: 'failure', status: 310 }); return abort(); }
          chains.set(requestId, [...previous, request.url]);
          if (request.method !== 'GET' || !/^https?:\/\//.test(request.url) || !['Document', 'Script', 'Stylesheet', 'XHR', 'Fetch'].includes(resourceType)) return abort();
          const response = new Promise((resolve) => pending.set(id, resolve));
          send({ type: 'request', id, url: request.url });
          const reply = await response;
          if (reply.denied) return abort();
          const resource = reply.resource;
          await session.send('Fetch.fulfillRequest', { requestId, responseCode: resource.status,
            responseHeaders: [{ name: 'content-type', value: resource.content_type }, { name: 'access-control-allow-origin', value: '*' }, ...(resource.location ? [{ name: 'location', value: resource.location }] : [])], body: resource.body });
        } catch { await abort(); }
      });
      await session.send('Fetch.enable', { patterns: [{ urlPattern: '*', requestStage: 'Request' }], handleAuthRequests: true });
    })();
    sessions.set(target, ready);
    return ready;
  }
  context.on('page', (page) => { void intercept(page).catch(() => {}); });
  const page = await context.newPage();
  await intercept(page);
  // An out-of-process frame has its own CDP target. Its network still fails closed
  // under OS denial while the interception session is being attached.
  page.on('frameattached', (frame) => { void intercept(frame).catch(() => {}); });
  const response = await page.goto(process.argv[1], { waitUntil: 'domcontentloaded', timeout: 20000 });
  if (response && !response.ok()) {
    send({ type: 'failure', status: response.status() });
    await browser.close(); return;
  }
  await page.waitForLoadState('networkidle', { timeout: 5000 }).catch(() => {});
  // Bound DOM settling even on pages with polling/background animations.
  await page.waitForTimeout(750);
  const html = await page.evaluate(() => {
    // Preserve structural challenge/login markers; strip visually hidden content before core extraction.
    for (const element of document.querySelectorAll('body *')) {
      const style = getComputedStyle(element);
      if (style.display === 'none' || style.visibility === 'hidden') element.setAttribute('hidden', '');
    }
    for (const element of document.querySelectorAll('script, style')) element.remove();
    return document.documentElement.outerHTML;
  });
  if (Buffer.byteLength(html) > 2 * 1024 * 1024) { send({ type: 'failure', status: 413 }); }
  else send({ type: 'done', page: { url: page.url(), html } });
  await browser.close();
}
setTimeout(() => { send({ type: 'failure', status: 408 }); process.exit(1); }, 29000).unref();
main().then(() => process.exit(0), (error) => { if (error?.name === 'TimeoutError') send({ type: 'failure', status: 408 }); process.exit(1); });
