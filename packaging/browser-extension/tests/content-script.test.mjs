import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

function makeHarness({schedule = setTimeout} = {}) {
  const state = {elements: [], seo: {}, activeElement: null, bodyText: 'Fixture ready', scrollX: 0, scrollY: 0};
  let listener;
  const element = (tagName, values = {}) => {
    const attrs = new Map(Object.entries(values.attrs || {}));
    const events = [];
    const item = {
      tagName: tagName.toUpperCase(),
      type: values.type || '',
      innerText: values.innerText || '',
      value: values.value || '',
      target: values.target || '',
      href: values.href || '',
      hidden: values.hidden || false,
      events,
      getAttribute(name) { return attrs.get(name) ?? null; },
      setAttribute(name, value) { attrs.set(name, String(value)); },
      get isConnected() { return state.elements.includes(this); },
      getBoundingClientRect() { return !this.isConnected || this.hidden ? {width: 0, height: 0} : {width: 20, height: 10}; },
      focus() { state.activeElement = this; },
      dispatchEvent(event) {
        events.push(event);
        if (event.type === 'change') this.changed = true;
        if (event.type === 'keydown') this.lastKey = event.key;
        return true;
      },
      click() { this.clicked = true; },
    };
    return item;
  };
  const document = {
    title: 'Rivo fixture',
    documentElement: {lang: 'zh-CN'},
    body: {get innerText() { return state.bodyText; }},
    get activeElement() { return state.activeElement; },
    querySelectorAll(selector) {
      if (selector === '[data-rivo-ref]') return state.elements.filter(item => item.getAttribute('data-rivo-ref'));
      if (Object.hasOwn(state.seo, selector)) return state.seo[selector];
      return state.elements;
    },
    querySelector(selector) {
      const match = selector.match(/^\[data-rivo-ref="([^"]+)"\]$/);
      return match ? state.elements.find(item => item.getAttribute('data-rivo-ref') === match[1]) || null : null;
    },
  };
  const chrome = {runtime: {onMessage: {addListener(fn) { listener = fn; }}}};
  const context = {
    chrome,
    document,
    window: {scrollBy(x, y) { state.scrollX += x; state.scrollY += y; }},
    location: {href: 'http://127.0.0.1/fixture'},
    URL,
    TextEncoder,
    getComputedStyle(item) { return {display: item.hidden ? 'none' : 'block', visibility: 'visible'}; },
    CSS: {escape(value) { return value; }},
    Event: class Event { constructor(type, init) { this.type = type; Object.assign(this, init); } },
    KeyboardEvent: class KeyboardEvent { constructor(type, init) { this.type = type; Object.assign(this, init); } },
    setTimeout: schedule,
  };
  Object.defineProperties(context, {
    scrollX: {get: () => state.scrollX},
    scrollY: {get: () => state.scrollY},
  });
  vm.runInNewContext(fs.readFileSync(new URL('../content-script.js', import.meta.url), 'utf8'), context);
  const call = (method, params = {}) => new Promise(resolve => {
    listener({method, params}, {}, value => resolve(JSON.parse(JSON.stringify(value))));
  });
  return {state, element, call};
}

test('content script snapshots visible safe controls and runs browser actions', async () => {
  const harness = makeHarness();
  const button = harness.element('button', {innerText: 'Click me'});
  const input = harness.element('input', {attrs: {placeholder: 'ordinary'}});
  const select = harness.element('select');
  const anchor = harness.element('a', {innerText: 'New window', target: '_blank', href: 'http://127.0.0.1/new'});
  const password = harness.element('input', {type: 'password', attrs: {'data-rivo-ref': 'rivo-900'}});
  const file = harness.element('input', {type: 'file', attrs: {'data-rivo-ref': 'rivo-901'}});
  const hidden = harness.element('button', {hidden: true, attrs: {'data-rivo-ref': 'rivo-902'}});
  harness.state.elements.push(button, input, select, anchor, password, file, hidden);

  const snapshot = await harness.call('browser_snapshot');
  assert.equal(snapshot.elements.some(item => item.tag === 'input' && item.type === 'password'), false);
  const refFor = (tag, predicate = () => true) => snapshot.elements.find(item => item.tag === tag && predicate(item)).ref;
  const buttonRef = refFor('button');
  const inputRef = refFor('input', item => item.placeholder === 'ordinary');
  const selectRef = refFor('select');
  const anchorRef = refFor('a');

  assert.deepEqual(await harness.call('browser_click', {ref: buttonRef}), {clicked: true});
  assert.equal(button.clicked, true);
  assert.equal((await harness.call('browser_type', {ref: inputRef, text: 'hello', clear: true})).value_length, 5);
  assert.equal(input.value, 'hello');
  assert.match((await harness.call('browser_type', {ref: inputRef, text: 'x'.repeat(16_385)})).error, /超过限制/);
  assert.equal(input.value, 'hello');
  assert.deepEqual(await harness.call('browser_select', {ref: selectRef, value: 'two'}), {value: 'two'});
  assert.equal(select.changed, true);

  await harness.call('browser_keypress', {key: 'Enter'});
  assert.equal(input.lastKey, 'Enter');
  assert.deepEqual(await harness.call('browser_scroll', {x: 3, y: 500}), {x: 3, y: 500});
  assert.deepEqual(await harness.call('browser_click', {ref: anchorRef}), {error: '禁止打开新窗口'});
  assert.equal(anchor.clicked, undefined);
});

test('content script exposes only a fixed readiness probe before snapshots', async () => {
  const harness = makeHarness();
  assert.deepEqual(await harness.call('browser_ready'), {ready: true});
});

test('content script waits for visible snapshot references and bounds timeout', async () => {
  const harness = makeHarness();
  const target = harness.element('button');
  harness.state.elements.push(target);
  const ref = (await harness.call('browser_snapshot')).elements[0].ref;
  harness.state.elements = [];
  setTimeout(() => {
    harness.state.elements.push(target);
  }, 20);
  assert.deepEqual(await harness.call('browser_wait', {ref, timeout_ms: 150}), {ref, ready: true});

  const disappearing = harness.element('button');
  harness.state.elements.push(disappearing);
  const missingRef = (await harness.call('browser_snapshot')).elements[0].ref;
  harness.state.elements = [];
  const timeout = await harness.call('browser_wait', {ref: missingRef, timeout_ms: 50});
  assert.match(timeout.error, /超时/);
  assert.deepEqual(await harness.call('browser_wait', {timeout_ms: 50}), {waited_ms: 50});
  assert.match((await harness.call('browser_wait', {ref: 'bad-ref'})).error, /元素引用/);
  assert.match((await harness.call('browser_wait', {timeout_ms: 'soon'})).error, /等待时间/);

  const delays = [];
  const capped = makeHarness({schedule: (callback, delay) => { delays.push(delay); return setTimeout(callback, 0); }});
  assert.deepEqual(await capped.call('browser_wait', {timeout_ms: 30_001}), {waited_ms: 30_000});
  assert.deepEqual(delays, [30_000]);
});

test('content script fails closed for unsafe element references and sensitive inputs', async () => {
  const harness = makeHarness();
  harness.state.elements.push(
    harness.element('input', {type: 'password', attrs: {'data-rivo-ref': 'rivo-900'}}),
    harness.element('input', {type: 'file', attrs: {'data-rivo-ref': 'rivo-901'}}),
    harness.element('button', {hidden: true, attrs: {'data-rivo-ref': 'rivo-902'}}),
  );
  for (const ref of ['rivo-900', 'rivo-901']) {
    assert.match((await harness.call('browser_type', {ref, text: 'secret'})).error, /引用/);
  }
  for (const ref of ['rivo-902', 'rivo-999']) {
    assert.match((await harness.call('browser_click', {ref})).error, /不存在或不可见/);
  }
  assert.match((await harness.call('browser_custom', {})).error, /未知页面方法/);
});

test('SEO snapshot preserves duplicate tags, headings and bounded JSON-LD summaries', async () => {
  const harness = makeHarness();
  const node = (attrs = {}, values = {}) => ({
    getAttribute(name) { return attrs[name] ?? null; },
    innerText: values.innerText || '',
    textContent: values.textContent || '',
  });
  harness.state.seo['meta[name]'] = [
    node({name: 'description', content: 'first description'}),
    node({name: 'description', content: 'second description'}),
    node({name: 'robots', content: 'index,follow'}),
    node({name: 'private-token', content: 'must not be extracted'}),
  ];
  harness.state.seo['meta[property^="og:"]'] = [
    node({property: 'og:title', content: 'Card title'}),
  ];
  harness.state.seo['meta[name^="twitter:"]'] = [];
  harness.state.seo['link[rel]'] = [
    node({rel: 'canonical', href: '/canonical'}),
    node({rel: 'alternate', hreflang: 'zh-CN', href: '/zh'}),
    node({rel: 'sitemap', href: '/sitemap.xml'}),
  ];
  harness.state.seo.h1 = [node({}, {innerText: 'Heading one'})];
  harness.state.seo.h2 = [node({}, {innerText: 'Section'})];
  harness.state.seo.h3 = [];
  harness.state.seo['script[type="application/ld+json"]'] = [
    node({}, {textContent: JSON.stringify({
      '@context': 'https://schema.org', '@type': 'WebSite', name: 'Example',
      url: 'https://example.test', description: 'Example site',
      image: [null, {}, {url: ''}, {url: '/assets/logo.png'}],
    })}),
    node({}, {textContent: '{ malformed'}),
  ];
  harness.state.seo.img = [node({alt: 'logo'}), node({alt: ''})];
  harness.state.seo['a[href]'] = [
    node({href: '/inside'}), node({href: 'https://outside.test/page'}),
  ];

  const result = await harness.call('browser_seo_snapshot');
  assert.deepEqual(result.meta.description, ['first description', 'second description']);
  assert.deepEqual(result.meta.robots, ['index,follow']);
  assert.equal(result.meta['private-token'], undefined);
  assert.deepEqual(result.canonical, ['http://127.0.0.1/canonical']);
  assert.deepEqual(result.hreflang, [{lang: 'zh-CN', href: 'http://127.0.0.1/zh'}]);
  assert.deepEqual(result.sitemaps, ['http://127.0.0.1/sitemap.xml']);
  assert.deepEqual(result.headings.h1, ['Heading one']);
  assert.equal(result.json_ld[0].type[0], 'WebSite');
  assert.deepEqual(result.json_ld[0].image, ['http://127.0.0.1/assets/logo.png']);
  assert.equal(JSON.stringify(result.json_ld).includes('/undefined'), false);
  assert.equal(result.json_ld_parse_errors, 1);
  assert.deepEqual(result.images, {scanned: 2, missing_alt: 1, truncated: false});
  assert.deepEqual(result.links, {scanned: 2, internal: 1, external: 1, truncated: false});
  assert.ok(new TextEncoder().encode(JSON.stringify(result)).length <= 48 * 1024);
});

test('SEO snapshot handles missing fields and caps oversized metadata output', async () => {
  const harness = makeHarness();
  const nodes = [];
  for (let index = 0; index < 3000; index += 1) {
    nodes.push({getAttribute(name) {
      if (name === 'property') return `og:fixture-${index}`;
      if (name === 'content') return 'x'.repeat(512);
      return null;
    }});
  }
  harness.state.seo['meta[name]'] = [];
  harness.state.seo['meta[property^="og:"]'] = nodes;
  harness.state.seo['meta[name^="twitter:"]'] = [];
  harness.state.seo['link[rel]'] = [];
  harness.state.seo.h1 = [];
  harness.state.seo.h2 = [];
  harness.state.seo.h3 = [];
  harness.state.seo['script[type="application/ld+json"]'] = [];
  harness.state.seo.img = [];
  harness.state.seo['a[href]'] = [];

  const result = await harness.call('browser_seo_snapshot');
  assert.deepEqual(result.canonical, []);
  assert.deepEqual(result.headings, {h1: [], h2: [], h3: []});
  assert.deepEqual(result.json_ld, []);
  assert.ok(Object.keys(result.open_graph).length < nodes.length);
  assert.ok(new TextEncoder().encode(JSON.stringify(result)).length <= 48 * 1024);
});
