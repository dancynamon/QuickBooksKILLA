// qbo-local sales-order capture (DECISIONS.md D42, docs/CUTOVER.md).
//
// Paste into the DevTools console of a signed-in QBO tab on
//   https://qbo.intuit.com/app/salesorders?tab=sales-orders
// while `qbo-local capture-sales-orders` is running on this Mac.
//
// It only reads. It lists every order on the Sales Orders grid, opens one so
// the page makes its own GetSalesOrder request, then replays that request
// once per order with the page's own session (web-app traffic: no developer
// API quota is spent). The raw answers go, unconverted, to the local
// receiver through a relay window on 127.0.0.1, because QBO's CSP blocks a
// direct fetch to localhost. Every rule about what the fields mean lives in
// Rust (src/sales_orders/capture.rs), where it is tested.
//
// If the relay window is blocked, click anywhere on the page and run
// `await __soDeliver()` in the console. As a last resort, `copy(__soCapture)`
// puts the capture on the clipboard for `qbo-local capture-sales-orders --capture FILE`.
(async () => {
  const PORT = __PORT__;
  const RECEIVER = `http://127.0.0.1:${PORT}`;
  const FORMAT = 'qbo-sales-order-capture/1';
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const log = (...a) => console.log('%c[so-capture]', 'font-weight:bold', ...a);

  if (location.origin !== 'https://qbo.intuit.com' || !/\/app\/salesorders/.test(location.pathname)) {
    throw new Error('Open https://qbo.intuit.com/app/salesorders?tab=sales-orders first, then paste again.');
  }

  // 1. Every order id on the grid, across every page.
  const ids = () => [...document.querySelectorAll('tr[data-item-id]')].map((tr) => tr.getAttribute('data-item-id'));
  const pager = (dir) => document.querySelector(`button[aria-label^="Go to ${dir} page"]`);
  const usable = (b) => b && !b.disabled && b.getAttribute('aria-disabled') !== 'true';
  const turn = async (dir) => {
    const before = ids().join();
    pager(dir).click();
    for (let i = 0; i < 60; i++) {
      await sleep(250);
      const now = ids().join();
      if (now && now !== before) return;
    }
    throw new Error(`The grid did not change after "${dir} page".`);
  };
  for (let i = 0; i < 60 && !ids().length; i++) await sleep(250);
  while (usable(pager('previous'))) await turn('previous');
  const listed = [];
  const add = () => ids().forEach((id) => { if (!listed.includes(id)) listed.push(id); });
  add();
  while (usable(pager('next'))) { await turn('next'); add(); }
  if (!listed.length) throw new Error('The Sales Orders grid is empty or still loading.');
  log(`${listed.length} orders on the grid`);

  // 2. The page's own GetSalesOrder request, captured by opening one order.
  let template = null;
  const pageFetch = window.fetch;
  window.fetch = function (input, init) {
    try {
      const url = typeof input === 'string' ? input : input.url;
      const body = init && init.body;
      if (!template && /commercecontrol/.test(url) && typeof body === 'string' && /GetSalesOrder"/.test(body)) {
        let headers = init.headers;
        headers = headers instanceof Headers ? Object.fromEntries(headers) : { ...headers };
        template = { url, body, headers, credentials: init.credentials };
      }
    } catch (_) { /* never break the page */ }
    return pageFetch.apply(this, arguments);
  };
  try {
    // Whichever order is on screen: paging has left the first ones behind.
    document.querySelector('tr[data-item-id] td').click();
    for (let i = 0; i < 80 && !template; i++) await sleep(250);
  } finally {
    window.fetch = pageFetch;
  }
  if (!template) throw new Error('Opening an order did not show a GetSalesOrder request; QBO may have changed.');

  const headers = { ...template.headers };
  for (const k of Object.keys(headers)) if (/^x-b3-/i.test(k)) delete headers[k];
  const realmId = headers['intuit-company-id'] || null;
  const getOrder = async (id) => {
    const body = JSON.parse(template.body);
    body.variables = { ...(body.variables || {}), id };
    const r = await pageFetch(template.url, { method: 'POST', headers, credentials: template.credentials, body: JSON.stringify(body) });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const j = await r.json();
    if (j.errors || !j.data || !j.data.result) throw new Error(JSON.stringify(j.errors || j).slice(0, 300));
    return j.data.result;
  };

  // 3. Every order, one at a time, gently.
  const orders = [];
  const errors = [];
  for (const [n, id] of listed.entries()) {
    try {
      orders.push(await getOrder(id));
    } catch (e) {
      errors.push({ id, error: String(e && e.message ? e.message : e) });
    }
    if ((n + 1) % 25 === 0) log(`${n + 1} / ${listed.length}`);
    await sleep(200);
  }
  log(`${orders.length} read, ${errors.length} failed`);

  window.__soCapture = JSON.stringify({ capture: FORMAT, realmId, capturedAt: new Date().toISOString(), listed, orders, errors });

  // 4. Hand it to the receiver through a relay window on 127.0.0.1.
  window.__soDeliver = async () => {
    const relay = window.open(`${RECEIVER}/`, 'qbo-local-capture');
    if (!relay) throw new Error('The relay window was blocked: click the page once, then run `await __soDeliver()`.');
    // The relay says 'got' as soon as a capture reaches it. Until then, keep
    // offering it: the window may still be loading, and QBO's opener policy
    // can stop the relay announcing itself.
    let got = false;
    let answer = null;
    const onMessage = (e) => {
      if (e.origin !== RECEIVER || typeof e.data !== 'string') return;
      if (e.data === 'got') got = true;
      else if (e.data.startsWith('result:')) answer = e.data.slice(7);
    };
    window.addEventListener('message', onMessage);
    try {
      for (let i = 0; i < 30 && !got && answer === null; i++) {
        try { relay.postMessage(window.__soCapture, RECEIVER); } catch (_) { /* still loading */ }
        await sleep(1000);
      }
      if (!got && answer === null) throw new Error(`No answer from ${RECEIVER}; is \`qbo-local capture-sales-orders\` running?`);
      for (let i = 0; i < 240 && answer === null; i++) await sleep(250);
    } finally {
      window.removeEventListener('message', onMessage);
    }
    log(answer === null ? 'sent; see the terminal for the result' : answer);
    return answer;
  };
  return window.__soDeliver();
})();
