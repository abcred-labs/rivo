const MAX_TEXT = 200;
const MAX_TYPE_TEXT = 16_384;
const MAX_WAIT_MS = 30_000;
const MAX_SEO_BYTES = 48 * 1024;
let observedElements = new Map();
const visible = (e) => {
  if (!e.isConnected) return false;
  const s = getComputedStyle(e), r = e.getBoundingClientRect();
  return s.display !== 'none' && s.visibility !== 'hidden' && r.width > 0 && r.height > 0;
};

function snapshot() {
  const elements = [];
  observedElements = new Map();
  let index = 0;
  for (const e of document.querySelectorAll('a,button,input,textarea,select,[role="button"],[contenteditable="true"]')) {
    if (!visible(e) || index >= 100) continue;
    if ((e.type || '').toLowerCase() === 'password' || (e.type || '').toLowerCase() === 'file') continue;
    const ref = `rivo-${++index}`;
    e.setAttribute('data-rivo-ref', ref);
    observedElements.set(ref, e);
    elements.push({ref, tag: e.tagName.toLowerCase(), text: (e.innerText || e.value || e.getAttribute('aria-label') || '').slice(0, MAX_TEXT), placeholder: (e.getAttribute('placeholder') || '').slice(0, MAX_TEXT) || null, href: (e.href || '').slice(0, 4096) || null});
  }
  return {title: document.title.slice(0, 512), url: location.href.slice(0, 4096), text: (document.body?.innerText || '').slice(0, 65536), elements};
}

function seoSnapshot() {
  const text = (value, limit = 256) => String(value || '').trim().slice(0, limit) || null;
  const absoluteUrl = (value) => {
    if (typeof value !== 'string' || !value.trim()) return null;
    try {
      const url = new URL(value, location.href);
      return ['http:', 'https:'].includes(url.protocol) ? url.href.slice(0, 2048) : null;
    } catch (_error) { return null; }
  };
  const collectProperties = (selector, keyAttribute, allowed = null) => {
    const result = {};
    let scanned = 0;
    for (const item of document.querySelectorAll(selector)) {
      if (scanned++ >= 2_000) break;
      const key = text(item.getAttribute(keyAttribute), 80)?.toLowerCase();
      const value = text(item.getAttribute('content'), 512);
      if (!key || !value || (allowed && !allowed.has(key))) continue;
      const values = result[key] || (result[key] = []);
      if (values.length < 4) values.push(value);
    }
    return result;
  };
  const meta = collectProperties('meta[name]', 'name', new Set([
    'description', 'robots', 'googlebot', 'viewport', 'author', 'keywords',
  ]));
  const openGraph = collectProperties('meta[property^="og:"]', 'property');
  const twitter = collectProperties('meta[name^="twitter:"]', 'name');
  const canonicalLinks = [];
  const hreflangLinks = [];
  let favicon = null;
  const sitemaps = [];
  for (const link of document.querySelectorAll('link[rel]')) {
    const rel = (link.getAttribute('rel') || '').toLowerCase().split(/\s+/);
    const href = absoluteUrl(link.getAttribute('href'));
    if (!href) continue;
    if (rel.includes('canonical') && canonicalLinks.length < 4) canonicalLinks.push(href);
    if (rel.includes('alternate') && link.getAttribute('hreflang') && hreflangLinks.length < 12) {
      hreflangLinks.push({lang: text(link.getAttribute('hreflang'), 32), href});
    }
    if (rel.includes('sitemap') && sitemaps.length < 4) sitemaps.push(href);
    if (!favicon && (rel.includes('icon') || rel.includes('shortcut'))) favicon = href;
  }
  const headings = {};
  const headingsTruncated = {};
  for (const level of ['h1', 'h2', 'h3']) {
    const limit = level === 'h1' ? 10 : 20;
    const values = [];
    let count = 0;
    for (const item of document.querySelectorAll(level)) {
      if (count++ >= limit) break;
      const value = text(item.innerText || item.textContent, 240);
      if (value) values.push(value);
    }
    headings[level] = values;
    headingsTruncated[level] = count > limit;
  }
  const jsonLd = [];
  let jsonLdParseErrors = 0;
  let jsonLdBytes = 0;
  const pushJsonLd = (value, depth = 0) => {
    if (depth > 6 || jsonLd.length >= 20 || !value || typeof value !== 'object') return;
    if (Array.isArray(value)) {
      for (const item of value) pushJsonLd(item, depth + 1);
      return;
    }
    if (value['@graph']) pushJsonLd(value['@graph'], depth + 1);
    const types = Array.isArray(value['@type']) ? value['@type'] : [value['@type']];
    const type = types.filter(item => typeof item === 'string').slice(0, 5).map(item => item.slice(0, 80));
    if (type.length) {
      const imageValues = Array.isArray(value.image) ? value.image : [value.image];
      const images = imageValues.slice(0, 4).map(image => typeof image === 'string' ? absoluteUrl(image) : absoluteUrl(image?.url)).filter(Boolean);
      jsonLd.push({
        type,
        name: text(value.name, 240),
        url: absoluteUrl(value.url),
        image: images,
        description: text(value.description, 400),
      });
    }
    for (const key of ['mainEntity', 'itemListElement', 'publisher', 'author', 'about', 'isPartOf']) {
      if (value[key]) pushJsonLd(value[key], depth + 1);
    }
  };
  let jsonLdScriptCount = 0;
  for (const script of document.querySelectorAll('script[type="application/ld+json"]')) {
    if (jsonLdScriptCount++ >= 16) break;
    const raw = script.textContent || '';
    jsonLdBytes += raw.length;
    if (raw.length > 64 * 1024 || jsonLdBytes > 256 * 1024) { jsonLdParseErrors += 1; continue; }
    try { pushJsonLd(JSON.parse(raw)); } catch (_error) { jsonLdParseErrors += 1; }
    if (jsonLd.length >= 20) break;
  }
  let missingAltImages = 0;
  let imageCount = 0;
  let imagesTruncated = false;
  for (const image of document.querySelectorAll('img')) {
    if (imageCount >= 5_000) { imagesTruncated = true; break; }
    imageCount += 1;
    if (!text(image.getAttribute('alt'), 1)) missingAltImages += 1;
  }
  let internalLinks = 0;
  let externalLinks = 0;
  let linkCount = 0;
  let linksTruncated = false;
  for (const anchor of document.querySelectorAll('a[href]')) {
    if (linkCount >= 5_000) { linksTruncated = true; break; }
    linkCount += 1;
    const href = absoluteUrl(anchor.getAttribute('href'));
    if (!href) continue;
    try {
      if (new URL(href).origin === new URL(location.href).origin) internalLinks += 1;
      else externalLinks += 1;
    } catch (_error) {}
  }
  const result = {
    url: String(location.href).slice(0, 2048),
    title: text(document.title, 512),
    lang: text(document.documentElement?.lang, 32),
    body_excerpt: text(document.body?.innerText, 600),
    meta,
    meta_scan_truncated: document.querySelectorAll('meta[name]').length > 2_000,
    canonical: canonicalLinks,
    hreflang: hreflangLinks,
    sitemaps,
    robots_txt_url: absoluteUrl('/robots.txt'),
    favicon,
    headings,
    headings_scan_truncated: headingsTruncated,
    open_graph: openGraph,
    twitter,
    json_ld: jsonLd,
    json_ld_parse_errors: jsonLdParseErrors,
    json_ld_scan_truncated: jsonLdScriptCount > 16,
    images: {scanned: imageCount, missing_alt: missingAltImages, truncated: imagesTruncated},
    links: {scanned: linkCount, internal: internalLinks, external: externalLinks, truncated: linksTruncated},
  };
  // Keep the returned JSON bounded even for pages with unusually large head
  // sections. Drop lowest-priority detail instead of emitting partial JSON.
  while (new TextEncoder().encode(JSON.stringify(result)).length > MAX_SEO_BYTES) {
    if (result.json_ld.length) result.json_ld.splice(Math.ceil(result.json_ld.length / 2));
    else if (Object.keys(result.open_graph).length) {
      for (const key of Object.keys(result.open_graph).slice(Math.ceil(Object.keys(result.open_graph).length / 2))) delete result.open_graph[key];
    } else if (Object.keys(result.twitter).length) {
      for (const key of Object.keys(result.twitter).slice(Math.ceil(Object.keys(result.twitter).length / 2))) delete result.twitter[key];
    } else if (Object.keys(result.meta).length) {
      for (const key of Object.keys(result.meta).slice(Math.ceil(Object.keys(result.meta).length / 2))) delete result.meta[key];
    } else if (result.hreflang.length) result.hreflang.splice(Math.ceil(result.hreflang.length / 2));
    else if (result.sitemaps.length) result.sitemaps.splice(Math.ceil(result.sitemaps.length / 2));
    else if (result.headings.h3.length) result.headings.h3.splice(Math.ceil(result.headings.h3.length / 2));
    else if (result.headings.h2.length) result.headings.h2.splice(Math.ceil(result.headings.h2.length / 2));
    else result.body_excerpt = result.body_excerpt?.slice(0, Math.floor(result.body_excerpt.length / 2)) || null;
  }
  return result;
}

function element(ref) {
  if (typeof ref !== 'string' || ref.length > 32 || !/^rivo-[1-9][0-9]*$/.test(ref)) {
    throw new Error('无效的元素引用');
  }
  const e = observedElements.get(ref);
  if (!e || !visible(e)) throw new Error('元素引用不存在或不可见');
  return e;
}

function waitForReference(ref, timeoutMs, sendResponse) {
  if (typeof ref !== 'string' || ref.length > 32 || !/^rivo-[1-9][0-9]*$/.test(ref)) {
    throw new Error('无效的元素引用');
  }
  const target = observedElements.get(ref);
  if (!target) throw new Error('元素引用未在当前快照中观察');
  const deadline = Date.now() + timeoutMs;
  const poll = () => {
    if (visible(target)) {
      sendResponse({ref, ready: true});
      return;
    }
    const remaining = deadline - Date.now();
    if (remaining <= 0) {
      sendResponse({error: '等待元素超时'});
      return;
    }
    setTimeout(poll, Math.min(100, remaining));
  };
  poll();
}

function boundedTimeoutMs(value) {
  if (value === undefined) return 1_000;
  if (!Number.isSafeInteger(value) || value < 0) throw new Error('无效的等待时间');
  return Math.max(50, Math.min(MAX_WAIT_MS, value));
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  try {
    const p = message.params || {};
    switch (message.method) {
      case 'browser_ready': return sendResponse({ready: true});
      case 'browser_snapshot': return sendResponse(snapshot());
      case 'browser_seo_snapshot': return sendResponse(seoSnapshot());
      case 'browser_click': { const e = element(p.ref); if ((e.target || e.form?.target || '').toLowerCase() === '_blank') throw new Error('禁止打开新窗口'); e.click(); return sendResponse({clicked: true}); }
      case 'browser_type': { const e = element(p.ref); if ((e.type || '').toLowerCase() === 'password' || (e.type || '').toLowerCase() === 'file') throw new Error('禁止操作 password/file 字段'); if (typeof p.text !== 'string' || p.text.length > MAX_TYPE_TEXT) throw new Error('输入文本超过限制'); e.focus(); if (p.clear) e.value = ''; e.value = `${e.value || ''}${p.text}`; e.dispatchEvent(new Event('input', {bubbles: true})); e.dispatchEvent(new Event('change', {bubbles: true})); return sendResponse({value_length: e.value.length}); }
      case 'browser_select': { const e = element(p.ref); if (e.tagName !== 'SELECT') throw new Error('目标不是 select'); e.value = p.value; e.dispatchEvent(new Event('change', {bubbles: true})); return sendResponse({value: e.value}); }
      case 'browser_keypress': { const e = document.activeElement || document.body; e.dispatchEvent(new KeyboardEvent('keydown', {key: p.key, bubbles: true})); e.dispatchEvent(new KeyboardEvent('keyup', {key: p.key, bubbles: true})); return sendResponse({key: p.key}); }
      case 'browser_scroll': window.scrollBy(p.x || 0, p.y || 0); return sendResponse({x: scrollX, y: scrollY});
      case 'browser_wait': {
        const timeoutMs = boundedTimeoutMs(p.timeout_ms);
        if (p.ref === undefined || p.ref === null || p.ref === '') {
          setTimeout(() => sendResponse({waited_ms: timeoutMs}), timeoutMs);
        } else {
          waitForReference(p.ref, timeoutMs, sendResponse);
        }
        return true;
      }
      default: throw new Error(`未知页面方法: ${message.method}`);
    }
  } catch (error) { sendResponse({error: String(error.message || error)}); }
  return true;
});
