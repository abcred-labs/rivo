import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

async function makeHarness({sendMessage, nativeHostAvailable = true} = {}) {
  let stored = {};
  let messageHandler;
  let connectCalls = 0;
  const sent = [];
  const listeners = new Set();
  const startupListeners = new Set();
  const installListeners = new Set();
  const tab = {id: 7, active: true, title: 'Test', url: 'https://example.test/', status: 'complete'};
  const updated = (changeInfo) => {
    for (const listener of listeners) listener(tab.id, changeInfo, {...tab});
  };
  const port = {
    onMessage: {addListener(fn) { messageHandler = fn; }},
    onDisconnect: {addListener() {}},
    postMessage(value) { sent.push(value); },
    disconnect() {},
  };
  const chrome = {
    storage: {local: {
      async get(keys) { return Object.fromEntries(keys.filter(k => k in stored).map(k => [k, stored[k]])); },
      async set(values) { stored = {...stored, ...values}; },
    }},
    runtime: {
      id: 'test-extension',
      onStartup: {addListener(fn) { startupListeners.add(fn); }},
      onInstalled: {addListener(fn) { installListeners.add(fn); }},
      connectNative() {
        connectCalls++;
        if (!nativeHostAvailable) throw new Error('native host is not registered');
        return port;
      },
    },
    tabs: {
      async query(query) {
        return query.active ? [{...tab}] : [{...tab}];
      },
      onUpdated: {
        addListener(fn) { listeners.add(fn); },
        removeListener(fn) { listeners.delete(fn); },
      },
      async update(id, value) {
        tab.status = 'loading';
        tab.url = value.url;
        updated({status: 'loading'});
        setTimeout(() => { tab.status = 'complete'; updated({status: 'complete'}); }, 0);
        return {...tab};
      },
      async get(id) { return {...tab}; },
      async sendMessage(tabId, request) {
        if (sendMessage) return sendMessage(tabId, request);
        if (request.method === 'browser_ready') return {ready: true};
        return {method: request.method, echoed: true};
      },
    },
  };
  let uuid = 0;
  const context = {
    chrome,
    navigator: {userAgent: 'Mozilla/5.0 Chrome/1'},
    crypto: {randomUUID: () => `uuid-${++uuid}`},
    URL,
    setTimeout,
    clearTimeout,
  };
  vm.runInNewContext(fs.readFileSync(new URL('../service-worker.js', import.meta.url), 'utf8'), context);
  await new Promise(resolve => setImmediate(resolve));
  return {
    messageHandler,
    sent,
    hello: sent[0],
    connectCalls: () => connectCalls,
    setNativeHostAvailable(value) { nativeHostAvailable = value; },
    fireStartup() { for (const listener of startupListeners) listener(); },
    fireInstalled() { for (const listener of installListeners) listener(); },
  };
}

function request(method, params = {}, deadlineMs = 1000) {
  return {
    protocol: 'rivo-browser-extension-v1',
    id: `request-${method}`,
    method,
    browser_instance: {browser_family: 'chrome', boot_id: 'boot-1'},
    params,
    deadline_ms: deadlineMs,
  };
}

test('service worker persists identity and binds successful responses', async () => {
  const harness = await makeHarness();
  assert.equal(harness.hello.protocol, 'rivo-browser-extension-v1');
  assert.equal(harness.hello.browser_family, 'chrome');
  assert.equal(harness.hello.installation_id, 'uuid-2');

  await harness.messageHandler(request('browser_list_tabs'));
  const response = harness.sent.at(-1);
  assert.equal(response.ok, true);
  assert.deepEqual(response.browser_instance, request('browser_list_tabs').browser_instance);
  assert.equal(response.result[0].id, '7');
});

test('service worker reconnects when the browser starts after the host becomes available', async () => {
  const harness = await makeHarness({nativeHostAvailable: false});
  assert.equal(harness.connectCalls(), 1);

  harness.setNativeHostAvailable(true);
  harness.fireStartup();
  await new Promise(resolve => setImmediate(resolve));

  assert.equal(harness.connectCalls(), 2);
  assert.equal(harness.sent[0].protocol, 'rivo-browser-extension-v1');
  assert.equal(harness.sent[0].boot_id, 'uuid-1');
});

test('service worker rejects unsafe navigation and unsupported operations', async () => {
  const harness = await makeHarness();
  for (const url of ['javascript:alert(1)', 'https://user:pass@example.test/']) {
    await harness.messageHandler(request('browser_navigate', {url}));
    assert.equal(harness.sent.at(-1).ok, false);
    assert.match(harness.sent.at(-1).error, /HTTP\(S\)/);
  }
  for (const method of ['browser_download', 'browser_upload', 'browser_execute_script']) {
    await harness.messageHandler(request(method));
    assert.equal(harness.sent.at(-1).ok, false);
    assert.match(harness.sent.at(-1).error, /安全策略/);
  }
});

test('service worker waits for navigation completion and content script readiness', async () => {
  const calls = [];
  const harness = await makeHarness({
    sendMessage: async (_tabId, request) => {
      calls.push(request.method);
      if (request.method === 'browser_ready') return {ready: true};
      return {title: 'loaded'};
    },
  });
  await harness.messageHandler(request('browser_navigate', {url: 'https://example.test/seo'}, 1000));
  const response = harness.sent.at(-1);
  assert.equal(response.ok, true);
  assert.equal(response.result.ready, true);
  assert.equal(response.result.url, 'https://example.test/seo');
  assert.ok(calls.includes('browser_ready'));
});

test('service worker rejects invalid deadlines and suppresses late results', async () => {
  let finishOperation;
  const harness = await makeHarness({
    sendMessage: () => new Promise(resolve => { finishOperation = resolve; }),
  });
  await harness.messageHandler(request('browser_snapshot', {}, 0));
  assert.equal(harness.sent.at(-1).ok, false);
  assert.match(harness.sent.at(-1).error, /deadline/);

  await harness.messageHandler(request('browser_snapshot', {}, 15));
  const timeoutResponse = harness.sent.at(-1);
  assert.equal(timeoutResponse.ok, false);
  assert.match(timeoutResponse.error, /deadline/);
  finishOperation?.({text: 'late result'});
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(harness.sent.at(-1), timeoutResponse);
});
