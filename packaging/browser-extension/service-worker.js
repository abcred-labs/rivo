const PROTOCOL = 'rivo-browser-extension-v1';
const MAX_REQUEST_DEADLINE_MS = 35000;
const bootId = crypto.randomUUID();
let port;
let reconnectTimer;

async function identity() {
  const stored = await chrome.storage.local.get(['profile_key', 'installation_id']);
  const installationId = stored.installation_id || crypto.randomUUID();
  if (!stored.installation_id) await chrome.storage.local.set({installation_id: installationId});
  return {protocol: PROTOCOL, browser_family: /Edg\//.test(navigator.userAgent) ? 'edge' : 'chrome', extension_id: chrome.runtime.id, installation_id: installationId, profile_key: stored.profile_key || installationId.slice(0, 8), boot_id: bootId, capabilities: ['tabs', 'storage', 'content_script']};
}

function connect() {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = undefined;
  }
  if (port) return;

  const scheduleReconnect = () => {
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = undefined;
      connect();
    }, 1000);
  };

  try {
    const nativePort = chrome.runtime.connectNative('com.rivo.browser');
    port = nativePort;
    nativePort.onMessage.addListener(request => handle(request, nativePort));
    nativePort.onDisconnect.addListener(() => {
      if (port === nativePort) {
        port = undefined;
        scheduleReconnect();
      }
    });
    identity().then(hello => nativePort.postMessage(hello)).catch(() => {
      try { nativePort.disconnect(); } catch (_error) {}
    });
  } catch (_error) {
    // Chrome throws synchronously when the host manifest is not registered
    // yet. Keep retrying so a host installed while the browser is running can
    // be discovered without requiring an extension reload.
    port = undefined;
    scheduleReconnect();
  }
}

async function sendTabMessage(tabId, message) {
  return await chrome.tabs.sendMessage(tabId, message);
}

async function waitForNavigation(tabId, navigate) {
  const loading = new Promise((resolve, reject) => {
    let sawLoading = false;
    const timer = setTimeout(() => finish(new Error('浏览器导航等待超时')), 15000);
    const finish = (error, tab) => {
      clearTimeout(timer);
      chrome.tabs.onUpdated.removeListener(onUpdated);
      if (error) reject(error);
      else resolve(tab);
    };
    const onUpdated = (updatedTabId, changeInfo, tab) => {
      if (updatedTabId !== tabId) return;
      if (changeInfo.status === 'loading') sawLoading = true;
      if (sawLoading && changeInfo.status === 'complete') finish(null, tab);
    };
    chrome.tabs.onUpdated.addListener(onUpdated);
    Promise.resolve().then(navigate).catch(error => finish(error));
  });
  await loading;
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    try {
      const ready = await sendTabMessage(tabId, {method: 'browser_ready', params: {}});
      if (ready?.ready === true) return;
    } catch (_error) {
      // `tabs.onUpdated=complete` may precede content script startup. Retry
      // only within a fixed bound; never redirect this request to another tab.
    }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error('浏览器页面控制脚本在导航完成后仍不可用');
}

function reply(targetPort, request, ok, result, error) {
  try {
    targetPort.postMessage({
      protocol: PROTOCOL,
      id: request.id,
      ok,
      result: result || {},
      ...(error ? {error} : {}),
      browser_instance: request.browser_instance,
    });
  } catch (_error) {
    // A disconnected port must never redirect a late response to a new host.
  }
}

async function perform(request) {
  const p = request.params || {};
  const httpUrl = (value) => {
    const url = new URL(value);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) {
      throw new Error('只允许无凭据 HTTP(S) 地址');
    }
    return url.toString();
  };
  if (request.method === 'browser_list_tabs') {
    return (await chrome.tabs.query({})).slice(0, 100).map(t => ({id: String(t.id), title: (t.title || '').slice(0, 512), url: (t.url || '').slice(0, 4096), selected: t.active === true}));
  }
  if (request.method === 'browser_select_tab') {
    const id = Number(p.id);
    if (!Number.isSafeInteger(id) || id < 0) throw new Error('无效的标签页 ID');
    await chrome.tabs.update(id, {active: true});
    return {id: String(id), selected: true};
  }
  if (request.method === 'browser_navigate') {
    const url = httpUrl(p.url);
    const tabs = await chrome.tabs.query({active: true, currentWindow: true});
    if (!tabs[0]) throw new Error('当前窗口没有可导航的标签页');
    await waitForNavigation(tabs[0].id, () => chrome.tabs.update(tabs[0].id, {url}));
    const current = await chrome.tabs.get(tabs[0].id);
    return {url: current.url || url, ready: true};
  }
  if (['browser_download', 'browser_upload', 'browser_execute_script'].includes(request.method)) {
    throw new Error('该浏览器操作已被安全策略拒绝');
  }
  const tabs = await chrome.tabs.query({active: true, currentWindow: true});
  if (!tabs[0]) throw new Error('当前窗口没有可操作的标签页');
  const result = await sendTabMessage(tabs[0].id, request);
  if (result?.error) throw new Error(result.error);
  return result;
}

async function handle(request, requestPort = port) {
  if (!request || request.protocol !== PROTOCOL || typeof request.id !== 'string' || !request.id) return;
  const deadlineMs = request.deadline_ms;
  if (!Number.isSafeInteger(deadlineMs) || deadlineMs < 1 || deadlineMs > MAX_REQUEST_DEADLINE_MS) {
    reply(requestPort, request, false, {}, '无效的请求 deadline');
    return;
  }

  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error('浏览器请求 deadline 已到')), deadlineMs);
  });
  try {
    const result = await Promise.race([perform(request), timeout]);
    reply(requestPort, request, true, result);
  } catch (error) {
    reply(requestPort, request, false, {}, String(error.message || error));
  } finally {
    clearTimeout(timer);
  }
}

chrome.runtime.onInstalled.addListener(connect);
chrome.runtime.onStartup.addListener(connect);
connect();
