// @author kongweiguang
/*
 * DOM / 异步 / XHR 行为回归测试（Node 内置 test runner，无第三方依赖）。
 *
 * 为什么需要这一层：纯函数测试只能证明算法正确，而本次修复的缺口全部在
 * “请求时序 + DOM 状态”这一层——刷新失败后重试是否真的重发、加载中导航是否生效、
 * 上传占位是否释放、取消是否会重入派发、非法确认体是否被当成成功。
 * 这里用一个最小假 DOM 与可控 fetch/XHR 驱动真实页面脚本的 __boot，
 * 断言渲染出来的文本和类名，而不是断言内部变量，避免测试和实现一起漂移。
 */
'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');

const app = require(path.join(__dirname, '..', '..', 'web', 'app.js'));

/* ------------------------------------------------------------------ */
/* 最小假 DOM                                                          */
/* ------------------------------------------------------------------ */

/**
 * 只实现页面脚本真正用到的 DOM 子集：getElementById、querySelectorAll、
 * createElement/createElementNS、classList、textContent、appendChild/removeChild、事件监听。
 * 之所以手写而不引第三方库，是为了让回归测试保持零依赖，并显式控制每个节点。
 */
class FakeClassList {
  constructor() {
    this.set = new Set();
  }

  add(name) { this.set.add(name); }

  remove(name) { this.set.delete(name); }

  toggle(name, force) {
    const on = force === undefined ? !this.set.has(name) : !!force;
    if (on) this.set.add(name);
    else this.set.delete(name);
    return on;
  }

  contains(name) { return this.set.has(name); }
}

class FakeNode {
  constructor(doc, tagName) {
    this.ownerDocument = doc;
    this.tagName = String(tagName).toUpperCase();
    this.children = [];
    this.parentNode = null;
    this.attributes = Object.create(null);
    this.classList = new FakeClassList();
    this.listeners = Object.create(null);
    this._text = '';
    this.style = {};
    this.disabled = false;
    this.hidden = false;
  }

  // 真实 DOM 里 className 与 classList 是同一份数据的两个视图；
  // 页面脚本两种写法都用，假 DOM 必须保持一致，否则断言会漏掉所有按 class 渲染的状态。
  get className() {
    return Array.from(this.classList.set).join(' ');
  }

  set className(value) {
    this.classList.set = new Set(String(value).split(/\s+/).filter(Boolean));
  }

  // 真实 DOM 中 html.lang 是属性 lang 的反射属性；页面用它标注语言，断言也应读属性。
  get lang() {
    return this.getAttribute('lang') || '';
  }

  set lang(value) {
    this.setAttribute('lang', value);
  }

  get firstChild() { return this.children.length ? this.children[0] : null; }

  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  removeChild(child) {
    const index = this.children.indexOf(child);
    if (index >= 0) this.children.splice(index, 1);
    child.parentNode = null;
    return child;
  }

  remove() {
    if (this.parentNode) this.parentNode.removeChild(this);
  }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
    if (name === 'hidden') this.hidden = true;
  }

  getAttribute(name) {
    return Object.prototype.hasOwnProperty.call(this.attributes, name) ? this.attributes[name] : null;
  }

  removeAttribute(name) {
    delete this.attributes[name];
    if (name === 'hidden') this.hidden = false;
  }

  addEventListener(type, handler) {
    if (!this.listeners[type]) this.listeners[type] = [];
    this.listeners[type].push(handler);
  }

  /** 同步触发监听器，用来模拟用户点击与 XHR 回调。 */
  dispatch(type, event) {
    const handlers = this.listeners[type] || [];
    for (const handler of handlers) handler(event || {});
  }

  click() { this.dispatch('click', {}); }

  get textContent() {
    if (this.children.length === 0) return this._text;
    return this.children.map((child) => child.textContent).join('');
  }

  set textContent(value) {
    this._text = value == null ? '' : String(value);
    this.children = [];
  }

  /** 深度优先查找，用于断言“界面上是否出现了某段文案”。 */
  find(predicate) {
    if (predicate(this)) return this;
    for (const child of this.children) {
      const found = child.find(predicate);
      if (found) return found;
    }
    return null;
  }

  findAll(predicate, out) {
    const result = out || [];
    if (predicate(this)) result.push(this);
    for (const child of this.children) child.findAll(predicate, result);
    return result;
  }
}

/** 以 index.html 的 id 列表预置节点，避免测试里手写整页结构。 */
const REQUIRED_IDS = [
  'session-chip', 'session-chip-text', 'lang-btn', 'theme-btn', 'disconnect-btn',
  'link-notice', 'link-notice-text',
  'link-notice-dismiss', 'connect-view', 'connect-spinner', 'connect-title',
  'connect-body', 'connect-retry', 'browse-view', 'browse-banner', 'browse-banner-text',
  'browse-banner-action', 'share-title', 'up-btn', 'up-btn-label', 'refresh-btn',
  'breadcrumbs', 'upload-btn',
  'file-input', 'upload-hint', 'upload-summary', 'uploads', 'uploads-title', 'cancel-all',
  'clear-done', 'upload-list', 'list-card', 'list-head', 'list-status', 'list-count',
  'list-retry', 'entries', 'list-foot', 'load-more', 'drop-overlay'
];

/** index.html 中初始自带 hidden 的元素 id：假 DOM 的初始可见性必须与真实页面一致。 */
const HIDDEN_ON_LOAD = new Set([
  'session-chip', 'disconnect-btn', 'connect-spinner', 'connect-retry',
  'browse-view', 'browse-banner', 'browse-banner-action', 'upload-summary',
  'uploads', 'cancel-all', 'clear-done', 'list-retry', 'list-foot', 'drop-overlay',
  'link-notice'
]);

function createFakeDocument() {
  const doc = {
    documentElement: null,
    body: null,
    hidden: false,
    listeners: Object.create(null),
    nodes: new Map(),
    createElement(tag) { return new FakeNode(doc, tag); },
    createElementNS(_ns, tag) { return new FakeNode(doc, tag); },
    createDocumentFragment() { return new FakeNode(doc, 'fragment'); },
    getElementById(id) { return doc.nodes.get(id) || null; },
    querySelectorAll() { return []; },
    addEventListener(type, handler) {
      if (!doc.listeners[type]) doc.listeners[type] = [];
      doc.listeners[type].push(handler);
    },
    dispatch(type, event) {
      for (const handler of doc.listeners[type] || []) handler(event || {});
    }
  };
  doc.documentElement = new FakeNode(doc, 'html');
  doc.body = new FakeNode(doc, 'body');
  for (const id of REQUIRED_IDS) {
    const node = new FakeNode(doc, id === 'file-input' ? 'input' : 'div');
    // index.html 里这些元素出厂自带 hidden；假 DOM 必须复刻真实初始状态，
    // 否则“重新加载后某元素是否可见”的断言会测到假 DOM 的空档而不是页面契约。
    if (HIDDEN_ON_LOAD.has(id)) node.setAttribute('hidden', '');
    doc.nodes.set(id, node);
  }
  doc.nodes.get('file-input').files = [];
  return doc;
}

/* ------------------------------------------------------------------ */
/* 可控 fetch / XHR / 计时器                                            */
/* ------------------------------------------------------------------ */

/**
 * 请求闸门：每个请求都挂起，由测试显式 resolve/reject。
 * 这样能精确构造“加载中再导航”“刷新失败后重试”“上传与列表并发”等时序，
 * 而不依赖真实网络延迟或 setTimeout 竞速。
 */
function createFetchGate() {
  const requests = [];
  function fetchImpl(url, init) {
    return new Promise((resolve, reject) => {
      requests.push({ url, init: init || {}, resolve, reject });
    });
  }
  return { fetchImpl, requests };
}

function jsonResponse(status, payload) {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: () => Promise.resolve(payload === undefined ? '' : JSON.stringify(payload))
  };
}

/** 假 XHR：把回调暴露给测试，便于模拟 onload/onerror/onabort 与同步抛错。 */
function createXhrHarness() {
  const instances = [];
  class FakeXhr {
    constructor() {
      this.upload = {};
      this.headers = {};
      this.aborted = false;
      this.sent = false;
      this.openError = null;
      this.sendError = null;
      this.status = 0;
      this.responseText = '';
      instances.push(this);
    }

    open(method, url) {
      if (this.openError) throw this.openError;
      this.method = method;
      this.url = url;
    }

    setRequestHeader(name, value) { this.headers[name] = value; }

    send(body) {
      if (this.sendError) throw this.sendError;
      this.sent = true;
      this.body = body;
    }

    abort() {
      this.aborted = true;
      if (this.onabort) this.onabort();
    }

    /** 模拟成功响应。 */
    respond(status, payload) {
      this.status = status;
      this.responseText = payload === undefined ? '' : (typeof payload === 'string' ? payload : JSON.stringify(payload));
      if (this.onload) this.onload();
    }
  }
  return { FakeXhr, instances };
}

/** 记录 setInterval/setTimeout 的假计时器，避免测试真的等秒。 */
function createTimers() {
  const intervals = [];
  const timeouts = [];
  return {
    intervals,
    timeouts,
    setInterval(fn, ms) { const id = { fn, ms }; intervals.push(id); return id; },
    clearInterval(id) { const i = intervals.indexOf(id); if (i >= 0) intervals.splice(i, 1); },
    setTimeout(fn, ms) { const id = { fn, ms }; timeouts.push(id); return id; },
    clearTimeout(id) { const i = timeouts.indexOf(id); if (i >= 0) timeouts.splice(i, 1); }
  };
}

/**
 * 受控假 AbortController：abort 同步触发 signal 的 abort 监听器并记录实例。
 * 之所以要假实现而不留 undefined：页面代码在“加载中再次导航”“结束会话”时会真实调用
 * controller.abort()，测试必须能断言“旧请求确实被中止”，以及被中止请求随后以
 * AbortError（或任意错误）拒绝时不污染当前列表——这是真实浏览器必然发生的时序。
 */
function createAbortHarness() {
  const controllers = [];
  class FakeAbortSignal {
    constructor(owner) {
      this.owner = owner;
      this.aborted = false;
      this.listeners = [];
    }

    addEventListener(type, fn) {
      if (type === 'abort') this.listeners.push(fn);
    }
  }
  class FakeAbortController {
    constructor() {
      this.signal = new FakeAbortSignal(this);
      controllers.push(this);
    }

    abort() {
      if (this.signal.aborted) return;
      this.signal.aborted = true;
      for (const fn of this.signal.listeners.slice()) fn({ type: 'abort' });
    }
  }
  return { FakeAbortController, controllers };
}

/** 构造 name 为 AbortError 的拒绝原因，与真实 fetch 被中止时的拒绝形态一致。 */
function abortError() {
  const error = new Error('aborted');
  error.name = 'AbortError';
  return error;
}

/**
 * 启动一个页面实例。默认用 URL 片段里的配对密钥自动配对成功，
 * 让测试直接落在“已进入会话、正在加载 root”的状态上。
 * expiresAt/token 可覆盖，便于构造“本地时钟已过期”的会话。
 */
async function bootPage(options) {
  const opts = options || {};
  const doc = createFakeDocument();
  const gate = createFetchGate();
  const xhr = createXhrHarness();
  const timers = createTimers();
  const abortHarness = createAbortHarness();
  const storage = new Map();
  const hash = opts.hash === undefined ? '#Ab3xY9_qLmN0pQrS' : opts.hash;

  // 需要控制“本地时钟”的用例显式传入 now：页面代码用的是全局 Date.now，
  // 这里替换后由用例通过 page.clock.value 推进时间，避免依赖真实等待或碰运气。
  let restoreClock = null;
  const clock = { value: opts.now === undefined ? Date.now() : opts.now };
  if (opts.now !== undefined) {
    const realNow = Date.now;
    Date.now = () => clock.value;
    restoreClock = () => { Date.now = realNow; };
  }

  const winListeners = Object.create(null);
  const historyCalls = [];
  const win = {
    location: { hash, pathname: '/', search: '', origin: 'http://127.0.0.1:8080', protocol: 'http:' },
    navigator: { language: opts.language || 'zh-CN' },
    history: {
      // 真实 replaceState 会同步改写地址栏；假 DOM 必须一并清空 hash，
      // 否则“新密钥不残留在地址栏”的断言会测到假实现的空档。
      replaceState(_state, _title, url) {
        historyCalls.push(url);
        win.location.hash = url && url.indexOf('#') >= 0 ? url.slice(url.indexOf('#')) : '';
      }
    },
    localStorage: {
      getItem: (key) => (storage.has(key) ? storage.get(key) : null),
      setItem: (key, value) => storage.set(key, value)
    },
    fetch: gate.fetchImpl,
    XMLHttpRequest: xhr.FakeXhr,
    AbortController: abortHarness.FakeAbortController,
    addEventListener(type, handler) {
      if (!winListeners[type]) winListeners[type] = [];
      winListeners[type].push(handler);
    },
    dispatch(type, event) {
      for (const handler of winListeners[type] || []) handler(event || {});
    },
    setInterval: timers.setInterval,
    clearInterval: timers.clearInterval,
    setTimeout: timers.setTimeout,
    clearTimeout: timers.clearTimeout
  };

  app.__boot(win, doc);

  // 配对请求是第一个请求；成功后页面会紧接着发出 root 列表请求。
  if (opts.paired !== false) {
    await settle();
    assert.equal(gate.requests.length, 1, '启动后应只发出配对请求');
    gate.requests[0].resolve(jsonResponse(200, {
      token: opts.token || 'stub-token',
      expiresAt: opts.expiresAt === undefined ? 4102444800 : opts.expiresAt
    }));
    await settle();
  }

  return {
    doc, win, gate, xhr, timers, aborts: abortHarness.controllers, storage,
    requests: gate.requests, clock, restoreClock, historyCalls
  };
}

/** 让挂起的 Promise 链走完：多个微任务轮次。 */
async function settle(rounds) {
  const count = rounds || 6;
  for (let i = 0; i < count; i += 1) await Promise.resolve();
}

/** 取最近一次列表请求。 */
function lastListRequest(requests) {
  for (let i = requests.length - 1; i >= 0; i -= 1) {
    if (requests[i].url.indexOf('/api/list') === 0) return requests[i];
  }
  return null;
}

function listRequests(requests) {
  return requests.filter((item) => item.url.indexOf('/api/list') === 0);
}

function pairRequests(page) {
  return page.requests.filter((item) => item.url === '/api/pair');
}

/**
 * 模拟浏览器“同源同路径、只换 fragment”的导航：不重载文档，只改 hash 并触发事件。
 * 这是真实 Chromium 的行为（实测不会执行新脚本），因此不能再用一次 __boot 代替；
 * 旧闭包、旧监听器与在途请求必须由同一份 state 自己处理。
 */
async function changeHash(page, hash) {
  page.win.location.hash = hash;
  page.win.dispatch('hashchange', {});
  await settle();
}

function textOf(doc, id) {
  const node = doc.getElementById(id);
  return node ? node.textContent : null;
}

function breadcrumbLabels(doc) {
  const container = doc.getElementById('breadcrumbs');
  return container.findAll((node) => node.tagName === 'BUTTON').map((node) => node.textContent);
}

const ROOT_PAYLOAD = {
  breadcrumbs: [{ id: 'root', name: 'Share' }],
  entries: [
    { id: 'd1', name: '2026-11', kind: 'directory', size: null, modified: null },
    { id: 'f1', name: 'notes.txt', kind: 'file', size: 4300, modified: 1762800000 }
  ],
  nextCursor: 'cursor-2'
};

/* ------------------------------------------------------------------ */
/* 列表：重试、导航、空状态                                             */
/* ------------------------------------------------------------------ */

test('首页刷新失败后点重试，按原请求重发并保留已确认的目录与面包屑', async () => {
  const page = await bootPage();
  const first = lastListRequest(page.requests);
  assert.ok(first, '进入会话后应请求 root');
  first.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  assert.deepEqual(breadcrumbLabels(page.doc), ['Share']);
  assert.match(textOf(page.doc, 'list-count'), /2 项/);

  // 点“重试”之前先制造一次刷新失败：上传后刷新走的是 reset+keep。
  page.doc.getElementById('load-more').click();
  await settle();
  const pageRequest = lastListRequest(page.requests);
  assert.match(pageRequest.url, /cursor=cursor-2/);
  pageRequest.resolve(jsonResponse(500, { error: { code: 'internal' } }));
  await settle();

  const retry = page.doc.getElementById('list-retry');
  assert.equal(retry.hidden, false, '失败后应显示重试按钮');
  const before = listRequests(page.requests).length;
  retry.click();
  await settle();

  const after = listRequests(page.requests);
  assert.equal(after.length, before + 1, '重试必须真的再发一次请求');
  assert.match(after[after.length - 1].url, /cursor=cursor-2/, '翻页失败应按同一游标重发');
  assert.deepEqual(breadcrumbLabels(page.doc), ['Share'], '失败不应破坏已确认的面包屑');
});

test('首页首次加载失败后重试仍会重发请求，不会变成空操作', async () => {
  const page = await bootPage();
  lastListRequest(page.requests).resolve(jsonResponse(0, null));
  await settle();

  assert.equal(page.doc.getElementById('list-retry').hidden, false);
  const before = listRequests(page.requests).length;
  page.doc.getElementById('list-retry').click();
  await settle();

  const after = listRequests(page.requests);
  assert.equal(after.length, before + 1, '首次加载失败后的重试不能被静默丢弃');
  assert.match(after[after.length - 1].url, /dir=root/);
});

test('加载中点击另一个目录：丢弃旧响应，采用最新导航', async () => {
  const page = await bootPage();
  lastListRequest(page.requests).resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  // 进入 d1 的请求挂起，然后立刻点击面包屑回到 root。
  page.doc.getElementById('entries').findAll((node) => node.className === 'entry-label is-dir')[0].click();
  await settle();
  const pendingSub = lastListRequest(page.requests);
  assert.match(pendingSub.url, /dir=d1/);

  page.doc.getElementById('breadcrumbs').findAll((node) => node.tagName === 'BUTTON')[0].click();
  await settle();
  const latest = lastListRequest(page.requests);
  assert.match(latest.url, /dir=root/, '加载中再次导航必须发起新请求');

  // 旧请求即使晚到也不能覆盖新结果。
  pendingSub.resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Share' }, { id: 'd1', name: '2026-11' }],
    entries: [],
    nextCursor: null
  }));
  await settle();
  latest.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  assert.deepEqual(breadcrumbLabels(page.doc), ['Share'], '应显示最新导航的结果');
  assert.match(textOf(page.doc, 'list-count'), /2 项/);
});

test('导航失败时保留已确认的目录，并能回到根', async () => {
  const page = await bootPage();
  lastListRequest(page.requests).resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  page.doc.getElementById('entries').findAll((node) => node.className === 'entry-label is-dir')[0].click();
  await settle();
  lastListRequest(page.requests).resolve(jsonResponse(404, { error: { code: 'dir_not_found' } }));
  await settle();

  assert.deepEqual(breadcrumbLabels(page.doc), ['Share'], '导航失败后仍应停在原目录');
  assert.match(textOf(page.doc, 'list-status'), /目录不存在/);

  const root = page.doc.getElementById('breadcrumbs').findAll((node) => node.tagName === 'BUTTON')[0];
  root.click();
  await settle();
  lastListRequest(page.requests).resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  assert.deepEqual(breadcrumbLabels(page.doc), ['Share']);
  assert.match(textOf(page.doc, 'list-count'), /2 项/);
});

test('加载中不显示“目录为空”，也不先清空已确认的条目', async () => {
  const page = await bootPage();
  lastListRequest(page.requests).resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  // 刷新当前目录（keep）后挂起：列表仍应显示原有条目，不能出现空状态。
  page.doc.getElementById('browse-banner-action').click();
  await settle();
  const entries = page.doc.getElementById('entries');
  assert.equal(entries.findAll((node) => node.classList.contains('list-empty')).length, 0,
    '加载中不应渲染空状态行');
  assert.match(entries.textContent, /notes\.txt/, '加载中应保留已确认的条目');
});

test('首次加载挂起时显示加载中而不是空目录', async () => {
  const page = await bootPage();
  await settle();
  const entries = page.doc.getElementById('entries');
  const emptyRow = entries.find((node) => node.classList.contains('list-empty'));
  assert.ok(emptyRow, '加载中应有占位行');
  assert.match(emptyRow.textContent, /加载中/);
  assert.doesNotMatch(emptyRow.textContent, /此目录为空/);
});

/* ------------------------------------------------------------------ */
/* 上传：确认体、占位释放、取消重入                                     */
/* ------------------------------------------------------------------ */

async function enterSessionWithRoot() {
  const page = await bootPage();
  lastListRequest(page.requests).resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  return page;
}

/** 通过 file-input change 事件注入文件，走真实 addFiles 路径。 */
function selectFiles(page, names) {
  selectFilesOn(page.doc, names);
  return page;
}

/** 在指定 document 上注入文件：会话切换后的新界面挂在新 document 上，注入必须跟着走。 */
function selectFilesOn(doc, names) {
  const input = doc.getElementById('file-input');
  input.files = names.map((name) => ({ name, size: 4096 }));
  input.dispatch('change', {});
  return doc;
}

function uploadNodes(page) {
  return uploadNodesOn(page.doc);
}

function uploadNodesOn(doc) {
  return doc.getElementById('upload-list').findAll((node) => node.classList.contains('upload-item'));
}

function uploadStatuses(page) {
  return uploadNodesOn(page.doc).map((li) => li.find((node) => node.classList.contains('upload-state')).textContent);
}

function uploadStatusesOn(doc) {
  return uploadNodesOn(doc).map((li) => li.find((node) => node.classList.contains('upload-state')).textContent);
}

/**
 * 在同一 window（共享 fetch 闸门与 XHR 记录）上用全新 document 重新配对并进入新会话。
 * 之所以不复用旧 document 再次 __boot：重复启动会向同一个 document 累加事件监听器，
 * 旧闭包与新会话的监听器混在同一节点上，行为与断言都无法归因；全新 document 让旧
 * 监听器随旧界面一起失效，新会话的所有交互都发生在干净的节点上。
 */
async function bootFreshPair(page, hash, token, expiresAt) {
  const doc = createFakeDocument();
  page.win.location.hash = hash;
  app.__boot(page.win, doc);
  await settle();
  const pairs = page.requests.filter((item) => item.url.indexOf('/api/pair') === 0);
  const pair = pairs[pairs.length - 1];
  assert.ok(pair, '重新启动后应发出配对请求');
  pair.resolve(jsonResponse(200, {
    token: token || 'fresh-token',
    expiresAt: expiresAt === undefined ? 4102444800 : expiresAt
  }));
  await settle();
  return doc;
}

test('2xx 但确认体不合法时不得显示“已提交”，而是结果待核对并刷新目录', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['alpha.txt']);
  const request = page.xhr.instances[0];
  assert.ok(request && request.sent, '应发出上传请求');

  request.respond(200, { ok: true });
  await settle();

  const statuses = uploadStatuses(page);
  assert.equal(statuses.length, 1);
  assert.doesNotMatch(statuses[0], /已提交/, '非法确认体不能被当成成功');
  assert.match(statuses[0], /无法确认结果/);
  assert.match(textOf(page.doc, 'upload-summary'), /1 个待核对/);

  const refresh = lastListRequest(page.requests);
  assert.ok(refresh, '待核对状态应触发目录刷新以观察真实结果');
});

test('合法确认体才标记已提交，并刷新目录', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['alpha.txt']);
  page.xhr.instances[0].respond(200, { name: 'alpha.txt', size: 4096 });
  await settle();

  assert.match(uploadStatuses(page)[0], /已提交/);
  assert.match(textOf(page.doc, 'upload-summary'), /已提交 1/);
  assert.ok(lastListRequest(page.requests), '提交后应刷新目录');
});

test('上传在途取消：给出结果待核对，不断言失败或成功', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['slow.bin']);
  const request = page.xhr.instances[0];

  // 点击该条目的取消按钮，走真实 abort 路径。
  const cancelButton = uploadNodes(page)[0].find((node) => node.tagName === 'BUTTON');
  cancelButton.click();
  await settle();

  assert.equal(request.aborted, true);
  const status = uploadStatuses(page)[0];
  assert.doesNotMatch(status, /已提交/);
  assert.doesNotMatch(status, /已取消/, '在途取消不能断言一定未提交');
  assert.match(status, /无法确认结果/);
});

test('全部取消不会因 onabort 重入而重新派发仍在排队的文件', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['a.bin', 'b.bin', 'c.bin', 'd.bin']);
  await settle();

  // 并发上限是 2，因此应先有 2 个在途、2 个排队。
  assert.equal(page.xhr.instances.length, 2, '并发上限内只应发出两个请求');

  page.doc.getElementById('cancel-all').click();
  await settle();

  assert.equal(page.xhr.instances.length, 2, '全部取消后不得再启动排队项');
  const statuses = uploadStatuses(page);
  assert.equal(statuses.length, 4);
  for (const status of statuses) {
    assert.doesNotMatch(status, /上传中|排队中/, '全部取消后不应有仍在传输或排队的条目');
  }
});

test('XHR open/send 同步抛错时释放占位并继续队列', async () => {
  const page = await enterSessionWithRoot();
  // 让第一个请求在 open 时同步抛错，第二个正常发出。
  page.win.XMLHttpRequest = class extends page.xhr.FakeXhr {
    open(...args) {
      super.open(...args);
      if (!this.__failedOnce) {
        this.__failedOnce = true;
        throw new Error('blocked');
      }
    }
  };
  selectFiles(page, ['x.bin', 'y.bin']);
  await settle();

  const statuses = uploadStatuses(page);
  assert.equal(statuses.length, 2);
  assert.match(statuses[0], /请求失败/, 'open 抛错应显示请求失败');
  assert.ok(!/排队中/.test(statuses[1]), '占位释放后第二个文件应被派发，而不是永远排队');
});

test('上传期间列表在加载时，刷新请求被合并而不是 400ms 后放弃', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['alpha.txt']);
  await settle();

  // 让列表进入加载并保持挂起，再让上传完成。
  page.doc.getElementById('browse-banner-action').click();
  await settle();
  const pendingList = lastListRequest(page.requests);
  page.xhr.instances[0].respond(200, { name: 'alpha.txt', size: 4096 });
  await settle();

  const countAfterUpload = listRequests(page.requests).length;
  pendingList.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  assert.ok(listRequests(page.requests).length > countAfterUpload,
    '列表加载结束后应补一次刷新，而不是静默放弃');
});

test('网络中断按不确定处理：提示待核对并刷新，不断言失败', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['flaky.bin']);
  const request = page.xhr.instances[0];
  request.onerror();
  await settle();

  const status = uploadStatuses(page)[0];
  assert.doesNotMatch(status, /请求失败|网络中断/, '网络中断不能断言一定失败');
  assert.match(status, /无法确认结果/);
  assert.ok(lastListRequest(page.requests), '待核对应触发刷新以核对真实结果');
});

test('不同失败码给出各自准确的提示', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['big.bin']);
  page.xhr.instances[0].respond(413, { error: { code: 'too_large' } });
  await settle();
  assert.match(uploadStatuses(page)[0], /超过大小限制/);

  selectFiles(page, ['conflict.bin']);
  page.xhr.instances[1].respond(409, { error: { code: 'conflict' } });
  await settle();
  assert.match(uploadStatuses(page)[1], /同名文件已存在/);
});

/* ------------------------------------------------------------------ */
/* 下载票据与本地化                                                     */
/* ------------------------------------------------------------------ */

test('abort 同步抛错时不得释放并发槽，也不得谎报已取消', async () => {
  const page = await enterSessionWithRoot();
  page.win.XMLHttpRequest = class extends page.xhr.FakeXhr {
    abort() {
      throw new Error('cannot abort');
    }
  };
  selectFiles(page, ['a.bin', 'b.bin', 'c.bin']);
  await settle();
  assert.equal(page.xhr.instances.length, 2);

  page.doc.getElementById('cancel-all').click();
  await settle();

  // 两个在途请求的 abort 都抛错：它们可能仍在发送，槽位必须保留，
  // 第三个文件只能继续排队；界面只能说待核对，不能说已取消。
  assert.equal(page.xhr.instances.length, 2, 'abort 抛错后不得启动排队项，否则真实并发会超限');
  const statuses = uploadStatuses(page);
  assert.equal(statuses.length, 3);
  assert.match(statuses[0], /无法确认结果/);
  assert.match(statuses[1], /无法确认结果/);
  assert.match(statuses[2], /已取消/, '从未发出的排队项可以确定取消');
  assert.doesNotMatch(statuses[0], /已取消/);
  assert.doesNotMatch(statuses[1], /已取消/);

  // 真实终止事件到达后才回收槽位，并派发下一个排队项。
  page.xhr.instances[0].aborted = true;
  page.xhr.instances[0].onabort();
  await settle();
  assert.equal(page.xhr.instances.length, 2, '全部取消后不应因一次终止就重新派发已取消的排队项');
});

test('请求已发出后的 status 0、408、5xx 与未知状态都只能给出待核对', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['a.bin', 'b.bin', 'c.bin', 'd.bin']);
  page.xhr.instances[0].respond(0, null);
  page.xhr.instances[1].respond(408, null);
  page.xhr.instances[2].respond(503, { error: { code: 'unavailable' } });
  page.xhr.instances[3].respond(418, null);
  await settle();

  const statuses = uploadStatuses(page);
  assert.equal(statuses.length, 4);
  for (const status of statuses) {
    assert.match(status, /无法确认结果/, '可能已提交的响应不能断言失败: ' + status);
    assert.doesNotMatch(status, /请求失败|网络中断|服务暂时不可用/, '不得把未知结果写成确定失败');
  }
});

test('单条取消时 abort 抛错仍保留并发槽，真实终止后才继续队列', async () => {
  const page = await enterSessionWithRoot();
  page.win.XMLHttpRequest = class extends page.xhr.FakeXhr {
    abort() {
      throw new Error('cannot abort');
    }
  };
  selectFiles(page, ['a.bin', 'b.bin', 'c.bin']);
  await settle();
  assert.equal(page.xhr.instances.length, 2);

  const cancelButton = uploadNodes(page)[0].find((node) => node.tagName === 'BUTTON' && node.textContent === '取消');
  cancelButton.click();
  await settle();

  // 第一条的 abort 抛错：它仍可能占着槽位，第三个文件必须继续排队。
  assert.equal(page.xhr.instances.length, 2, 'abort 抛错不能把槽位让给排队项');
  assert.match(uploadStatuses(page)[0], /无法确认结果/);
  assert.doesNotMatch(uploadStatuses(page)[0], /已取消/);
  assert.match(uploadStatuses(page)[2], /排队中/);
  // abort 抛错后仍允许再次尝试取消，不能把这条记录变成无法操作。
  assert.equal(cancelButton.hidden, false);

  // 另一条请求真实完成并释放自己的槽位后，第三个文件才被派发。
  page.xhr.instances[1].respond(200, { name: 'b.bin', size: 4096 });
  await settle();
  assert.equal(page.xhr.instances.length, 3, '真实终止释放槽位后应继续队列');
  assert.doesNotMatch(uploadStatuses(page)[2], /排队中/);
});

test('清除已完成保留结果待核对记录，并提供逐项移除入口', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['done.txt', 'unknown.txt']);
  page.xhr.instances[0].respond(200, { name: 'done.txt', size: 4096 });
  await settle();
  page.xhr.instances[1].respond(200, { ok: true });
  await settle();

  page.doc.getElementById('clear-done').click();
  await settle();

  const statuses = uploadStatuses(page);
  assert.equal(statuses.length, 1, '待核对记录不能被无提示丢弃');
  assert.match(statuses[0], /无法确认结果/);
  assert.match(textOf(page.doc, 'upload-summary'), /1 个待核对/);

  const removeButton = uploadNodes(page)[0].find((node) => node.tagName === 'BUTTON' && node.textContent === '移除记录');
  assert.ok(removeButton, '待核对记录应提供手动移除记录按钮');
  assert.equal(removeButton.hidden, false);
  assert.match(removeButton.getAttribute('aria-label'), /移除记录: unknown\.txt/);
  // 必须说明只移除本页记录、不删除服务器文件，避免被读成“删除文件”。
  assert.match(removeButton.getAttribute('title'), /不会删除/);
  assert.match(removeButton.getAttribute('aria-label'), /不会删除/);
  // 清除按钮仍在时，必须说明待核对记录会保留，避免用户以为被静默丢弃。
  const clearDone = page.doc.getElementById('clear-done');
  assert.match(clearDone.getAttribute('title'), /待核对/);

  // 切到英文后入口仍是可聚焦按钮，文案与无障碍名称一起更新。
  page.doc.getElementById('lang-btn').click();
  await settle();
  const englishRemove = uploadNodes(page)[0]
    .find((node) => node.tagName === 'BUTTON' && node.textContent === 'Remove record');
  assert.ok(englishRemove, '英文界面也应提供移除记录入口');
  assert.match(englishRemove.getAttribute('aria-label'), /Remove record: unknown\.txt/);
  assert.match(englishRemove.getAttribute('title'), /no file on the server is deleted/);
  assert.equal(englishRemove.hidden, false);
  englishRemove.click();
  await settle();

  assert.equal(uploadStatuses(page).length, 0);
  assert.equal(page.doc.getElementById('uploads').hidden, true);
});

test('列表响应最后面包屑与请求目录不一致时按协议错误保留上次列表', async () => {
  const page = await enterSessionWithRoot();
  const beforeCount = textOf(page.doc, 'list-count');

  page.doc.getElementById('entries').findAll((node) => node.className === 'entry-label is-dir')[0].click();
  await settle();
  const mismatched = lastListRequest(page.requests);
  assert.match(mismatched.url, /dir=d1/);
  mismatched.resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Share' }, { id: 'other', name: 'Elsewhere' }],
    entries: [{ id: 'x', name: 'wrong.txt', kind: 'file', size: 1, modified: null }],
    nextCursor: null
  }));
  await settle();

  assert.deepEqual(breadcrumbLabels(page.doc), ['Share'], '错位响应不得改写面包屑');
  assert.equal(textOf(page.doc, 'list-count'), beforeCount, '错位响应不得替换列表');
  assert.doesNotMatch(page.doc.getElementById('entries').textContent, /wrong\.txt/);
  assert.match(textOf(page.doc, 'list-status'), /不符合约定/);

  // 重试必须仍指向原请求目录。
  const retry = page.doc.getElementById('list-retry');
  assert.equal(retry.hidden, false);
  const before = listRequests(page.requests).length;
  retry.click();
  await settle();
  const after = listRequests(page.requests);
  assert.equal(after.length, before + 1);
  assert.match(after[after.length - 1].url, /dir=d1/);
});

test('翻页响应最后面包屑与当前目录不一致时拒绝合并', async () => {
  const page = await enterSessionWithRoot();
  const beforeCount = textOf(page.doc, 'list-count');

  page.doc.getElementById('load-more').click();
  await settle();
  const pageRequest = lastListRequest(page.requests);
  assert.match(pageRequest.url, /cursor=cursor-2/);
  pageRequest.resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Share' }, { id: 'd1', name: '2026-11' }],
    entries: [{ id: 'z', name: 'other-dir.txt', kind: 'file', size: 2, modified: null }],
    nextCursor: null
  }));
  await settle();

  assert.equal(textOf(page.doc, 'list-count'), beforeCount, '错位翻页响应不得追加条目');
  assert.doesNotMatch(page.doc.getElementById('entries').textContent, /other-dir\.txt/);
  assert.match(textOf(page.doc, 'list-status'), /不符合约定/);
});

test('拒绝非 /download/<ticket> 的票据路径，不把任意同源地址交给浏览器', async () => {
  const page = await enterSessionWithRoot();
  const downloadButton = page.doc.getElementById('entries')
    .findAll((node) => node.tagName === 'BUTTON' && node.textContent === '下载')[0];
  assert.ok(downloadButton, '应有下载按钮');
  downloadButton.click();
  await settle();

  const ticketRequest = page.requests.filter((item) => item.url.indexOf('/api/download-ticket') === 0)[0];
  assert.ok(ticketRequest, '应先换取票据');
  ticketRequest.resolve(jsonResponse(200, { url: '/index.html' }));
  await settle();

  assert.match(textOf(page.doc, 'list-status'), /下载未开始/, '非票据路径应判为下载未开始');
  assert.equal(page.doc.body.findAll((node) => node.tagName === 'A').length, 0,
    '非法票据不应生成下载链接');
});

test('下载失败按错误码提示，不泄漏令牌', async () => {
  const page = await enterSessionWithRoot();
  const downloadButton = page.doc.getElementById('entries')
    .findAll((node) => node.tagName === 'BUTTON' && node.textContent === '下载')[0];
  downloadButton.click();
  await settle();

  const ticketRequest = page.requests.filter((item) => item.url.indexOf('/api/download-ticket') === 0)[0];
  ticketRequest.resolve(jsonResponse(410, { error: { code: 'ticket_expired', message: 'Bearer stub-token' } }));
  await settle();

  const status = textOf(page.doc, 'list-status');
  assert.match(status, /下载票据已过期/, '应按错误码给出准确提示');
  assert.doesNotMatch(status, /stub-token/, '状态行不得泄漏令牌');
});

test('切换语言后标签与列表文案一起更新', async () => {
  const page = await enterSessionWithRoot();
  page.doc.getElementById('theme-btn').click();
  await settle();
  assert.equal(page.doc.getElementById('theme-btn').textContent, '浅色');

  page.doc.getElementById('lang-btn').click();
  await settle();

  assert.equal(textOf(page.doc, 'upload-btn'), 'Choose files');
  assert.equal(textOf(page.doc, 'load-more'), 'Load more');
  assert.match(textOf(page.doc, 'list-count'), /2 items/);
  assert.equal(page.doc.documentElement.getAttribute('lang'), 'en');
  assert.equal(page.doc.getElementById('theme-btn').textContent, 'Light',
    '主题按钮标签也必须跟随语言切换');
});

test('切换主题后标签更新，再次切换回到自动', async () => {
  const page = await enterSessionWithRoot();
  const themeBtn = page.doc.getElementById('theme-btn');
  themeBtn.click();
  await settle();
  assert.equal(themeBtn.textContent, '浅色');
  themeBtn.click();
  await settle();
  assert.equal(themeBtn.textContent, '深色');
  themeBtn.click();
  await settle();
  assert.equal(themeBtn.textContent, '自动');
  assert.equal(page.doc.documentElement.getAttribute('data-theme'), null);
});

/* ------------------------------------------------------------------ */
/* 会话：本地时间不能断言服务端失效                                     */
/* ------------------------------------------------------------------ */

/**
 * 进入一个“本地时钟即将过期”的会话：配对时给 10 秒有效期，进入会话后再把假时钟
 * 推过有效期。这样能精确区分“进入时已过期”的自动校验和“tick 才发现过期”的场景，
 * 也不会依赖真实等待。
 */
async function enterExpiredClockSession() {
  const now = 1_700_000_000_000;
  const page = await bootPage({ now, expiresAt: now / 1000 + 10 });
  lastListRequest(page.requests).resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  page.clock.value = now + 11_000;
  return page;
}

function sessionRequests(page) {
  return page.requests.filter((item) => item.url.indexOf('/api/session') === 0);
}

/** 每条用例结束都恢复真实时钟，避免一个用例的假时间污染后续用例。 */
function restoreClock(page) {
  if (page.restoreClock) page.restoreClock();
}

test('本地倒计时归零不直接结束会话，只触发一次权威校验', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;

    page.timers.intervals[0].fn();
    await settle();

    assert.equal(page.doc.getElementById('browse-view').hidden, false, '本地时间过期不应把页面踢回连接页');
    assert.match(textOf(page.doc, 'session-chip-text'), /正在确认/, '本地时间归零只能说正在确认，不能说已过期');
    const requests = sessionRequests(page);
    assert.equal(requests.length, before + 1, '应发起且只发起一次权威会话校验');
    assert.match(requests[requests.length - 1].url, /\/api\/session/);

    // 在途期间每秒 tick 都不能并发再发。
    page.timers.intervals[0].fn();
    page.timers.intervals[0].fn();
    await settle();
    assert.equal(sessionRequests(page).length, before + 1, '在途检查不能被每秒 tick 叠加成并发请求');

    // 校验返回 200 且给出新的过期时间：会话继续有效。
    requests[requests.length - 1].resolve(jsonResponse(200, { expiresAt: 4102444800 }));
    await settle();
    assert.equal(page.doc.getElementById('browse-view').hidden, false);
    assert.doesNotMatch(textOf(page.doc, 'session-chip-text'), /正在确认/);
  } finally {
    restoreClock(page);
  }
});

test('服务端仍返回已过去的时间时不得在成功回调里立刻递归请求', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;

    page.timers.intervals[0].fn();
    await settle();
    sessionRequests(page)[before].resolve(jsonResponse(200, { expiresAt: 1000 }));
    await settle();

    assert.equal(sessionRequests(page).length, before + 1, '成功回调不能立即递归发请求');
    assert.match(textOf(page.doc, 'session-chip-text'), /正在确认/, '仍无法确认时继续显示正在确认');

    // 冷却期内每秒 tick 不再请求。
    for (let i = 0; i < 5; i += 1) page.timers.intervals[0].fn();
    await settle();
    assert.equal(sessionRequests(page).length, before + 1, '冷却期内 tick 不得请求');

    // 周期定时器（第二个 interval）不受冷却限制，会正常再问一次。
    page.timers.intervals[1].fn();
    await settle();
    assert.equal(sessionRequests(page).length, before + 2, '周期检查不应被冷却误禁');
  } finally {
    restoreClock(page);
  }
});

test('权威校验失败后不会每秒重发，周期检查与手动重试仍可继续', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;

    page.timers.intervals[0].fn();
    await settle();
    sessionRequests(page)[before].resolve(jsonResponse(503, { error: { code: 'unavailable' } }));
    await settle();

    assert.equal(sessionRequests(page).length, before + 1, '失败后不能在回调里立刻重发');
    for (let i = 0; i < 5; i += 1) page.timers.intervals[0].fn();
    await settle();
    assert.equal(sessionRequests(page).length, before + 1, '失败后的冷却期内 tick 不得请求');

    page.timers.intervals[1].fn();
    await settle();
    assert.equal(sessionRequests(page).length, before + 2, '一次失败后周期检查仍应能再问');
  } finally {
    restoreClock(page);
  }
});

test('手动重试在检查在途时只排队一次，落地后补发一次', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;

    page.timers.intervals[0].fn();
    await settle();
    const inFlight = sessionRequests(page)[before];
    assert.ok(inFlight);

    // 连点两次手动重试：不能并发，也不能丢点击。
    page.doc.getElementById('browse-banner-action').click();
    page.doc.getElementById('browse-banner-action').click();
    await settle();
    assert.equal(sessionRequests(page).length, before + 1, '在途时手动重试不得并发');

    inFlight.resolve(jsonResponse(200, { expiresAt: 4102444800 }));
    await settle();
    assert.equal(sessionRequests(page).length, before + 2, '排队的重试应在当前检查落地后补发一次');
  } finally {
    restoreClock(page);
  }
});

test('恢复可见时只做一次轻量校验，快速切换不会形成请求风暴', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;

    page.doc.hidden = false;
    page.doc.dispatch('visibilitychange');
    await settle();
    assert.equal(sessionRequests(page).length, before + 1, '恢复可见应发起一次校验');
    const pending = sessionRequests(page)[before];

    // 在途期间再次恢复可见不能并发；落地后短时间内再次切换也被最小间隔挡住。
    page.doc.dispatch('visibilitychange');
    await settle();
    assert.equal(sessionRequests(page).length, before + 1);
    pending.resolve(jsonResponse(200, { expiresAt: 4102444800 }));
    await settle();
    page.doc.dispatch('visibilitychange');
    await settle();
    assert.equal(sessionRequests(page).length, before + 1, '快速切换标签不应每次恢复都请求');
  } finally {
    restoreClock(page);
  }
});

test('权威接口返回 401 时才结束会话', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;

    page.timers.intervals[0].fn();
    await settle();
    sessionRequests(page)[before].resolve(jsonResponse(401, { error: { code: 'session_expired' } }));
    await settle();

    assert.equal(page.doc.getElementById('connect-view').hidden, false, '401 后应回到连接页');
    assert.equal(page.doc.getElementById('browse-view').hidden, true);
  } finally {
    restoreClock(page);
  }
});

test('旧会话迟到的 401 不得结束新会话', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;
    page.timers.intervals[0].fn();
    await settle();
    const stale = sessionRequests(page)[before];

    // 断开后用新密钥在全新 document 上重新配对；同一 document 重复 __boot 会累加事件监听器，
    // 旧闭包会把行为污染到新会话的断言上，这里改为干净的新界面。
    page.doc.getElementById('disconnect-btn').click();
    await settle();
    const newDoc = await bootFreshPair(page, '#Zz9yY8xX7wW6vV5u', 'fresh-token');
    assert.equal(newDoc.getElementById('browse-view').hidden, false, '新会话应已进入浏览页');

    stale.resolve(jsonResponse(401, { error: { code: 'session_expired' } }));
    await settle();
    assert.equal(newDoc.getElementById('browse-view').hidden, false, '旧会话迟到的 401 不得结束新会话');
    assert.equal(newDoc.getElementById('connect-view').hidden, true);
  } finally {
    restoreClock(page);
  }
});

/* ------------------------------------------------------------------ */
/* 配对：临时失败保留密钥，未知响应不谎称无效                           */
/* ------------------------------------------------------------------ */

function pairRequest(page) {
  return page.requests.filter((item) => item.url.indexOf('/api/pair') === 0)[0];
}

function lastPairRequest(page) {
  const all = page.requests.filter((item) => item.url.indexOf('/api/pair') === 0);
  return all[all.length - 1];
}

for (const status of [0, 408, 503]) {
  test(`配对返回 ${status} 视为暂时不可用并保留密钥可重试`, async () => {
    const page = await bootPage({ paired: false });
    await settle();
    const first = pairRequest(page);
    assert.ok(first);
    first.resolve(jsonResponse(status, status === 0 ? null : { error: { code: 'unavailable' } }));
    await settle();

    assert.match(textOf(page.doc, 'connect-body'), /无法连接服务|暂时无法/);
    assert.equal(page.doc.getElementById('connect-retry').hidden, false, '临时失败应允许手动重试');

    const before = page.requests.length;
    page.doc.getElementById('connect-retry').click();
    await settle();
    assert.equal(page.requests.length, before + 1, '重试应真的再发配对请求');
    const retry = lastPairRequest(page);
    assert.ok(retry);
    assert.equal(JSON.parse(retry.init.body).secret, 'Ab3xY9_qLmN0pQrS', '重试必须复用内存里的原密钥');
  });
}

test('配对返回 429 是限流不是无法连接：明确限流并保留密钥可重试', async () => {
  const page = await bootPage({ paired: false });
  await settle();
  pairRequest(page).resolve(jsonResponse(429, { error: { code: 'busy' } }));
  await settle();

  const body = textOf(page.doc, 'connect-body');
  assert.doesNotMatch(body, /无法连接服务/, '429 能到达服务端，不得宣称无法连接');
  assert.doesNotMatch(body, /仍在运行/, '不得引导用户排查一个在线的服务');
  assert.match(body, /限流/, '必须明确告知用户当前是限流');
  assert.match(body, /稍等|重试/, '必须说明这是可等待重试的暂时状态');
  assert.equal(page.doc.getElementById('connect-retry').hidden, false, '限流后应保留重试入口');

  // 英文界面同样使用限流文案，且不承诺具体等待时长。
  page.doc.getElementById('lang-btn').click();
  await settle();
  const englishBody = textOf(page.doc, 'connect-body');
  assert.match(englishBody, /rate limiting/i);
  assert.doesNotMatch(englishBody, /Cannot reach the service/, '英文限流文案不得与无法连接混淆');

  // 重试必须复用内存里的原密钥：429 不是链接失效，密钥不能被丢弃。
  const before = page.requests.length;
  page.doc.getElementById('connect-retry').click();
  await settle();
  assert.equal(page.requests.length, before + 1);
  assert.equal(JSON.parse(lastPairRequest(page).init.body).secret, 'Ab3xY9_qLmN0pQrS', '429 后重试必须保留原密钥');
});

test('配对返回未知状态码或非法确认体时不断言链接失效，保留密钥', async () => {
  const page = await bootPage({ paired: false });
  await settle();
  pairRequest(page).resolve(jsonResponse(418, null));
  await settle();

  assert.match(textOf(page.doc, 'connect-body'), /无法确认/);
  assert.equal(page.doc.getElementById('connect-retry').hidden, false);

  const before = page.requests.length;
  page.doc.getElementById('connect-retry').click();
  await settle();
  assert.equal(page.requests.length, before + 1);
  lastPairRequest(page).resolve(jsonResponse(200, { token: '' }));
  await settle();
  assert.match(textOf(page.doc, 'connect-body'), /无法确认/, '2xx 但确认体不可用不能判定链接无效');
  assert.equal(page.doc.getElementById('connect-retry').hidden, false);
});

test('配对返回 403/400 等非密钥错误时不能谎称链接失效', async () => {
  for (const [status, code] of [[403, 'forbidden'], [400, 'bad_request']]) {
    const page = await bootPage({ paired: false });
    await settle();
    pairRequest(page).resolve(jsonResponse(status, { error: { code } }));
    await settle();

    assert.match(textOf(page.doc, 'connect-body'), /无法确认/, `状态 ${status} 不代表链接失效`);
    assert.equal(page.doc.getElementById('connect-retry').hidden, false, '应保留密钥与重试入口');

    const before = page.requests.length;
    page.doc.getElementById('connect-retry').click();
    await settle();
    assert.equal(page.requests.length, before + 1, '重试应真的再发配对请求');
  }
});

test('配对明确返回 invalid_secret 时才判定链接无效并清除密钥', async () => {
  const page = await bootPage({ paired: false });
  await settle();
  pairRequest(page).resolve(jsonResponse(401, { error: { code: 'invalid_secret' } }));
  await settle();

  assert.match(textOf(page.doc, 'connect-body'), /无效或已过期/);
  assert.equal(page.doc.getElementById('connect-retry').hidden, true, '链接无效不应再提供自动重试');
});

test('配对在途时连点重试不并发，且旧响应不能覆盖新结果', async () => {
  const page = await bootPage({ paired: false });
  await settle();
  const first = pairRequest(page);

  // 状态仍是 pairing，重试按钮隐藏；直接派发点击也不能并发。
  page.doc.getElementById('connect-retry').click();
  await settle();
  assert.equal(page.requests.length, 1, '配对在途时不得并发第二次请求');

  first.resolve(jsonResponse(200, { token: 'new-token', expiresAt: 4102444800 }));
  await settle();
  assert.equal(page.doc.getElementById('browse-view').hidden, false, '成功后进入浏览页');
  assert.equal(page.requests.filter((item) => item.url.indexOf('/api/list') === 0).length, 1);
});

/* ------------------------------------------------------------------ */
/* 上传：会话代次与过时回调                                             */
/* ------------------------------------------------------------------ */

test('非文件拖放不触发上传，也不让浏览器把页面导航走', async () => {
  const page = await enterSessionWithRoot();
  const before = page.xhr.instances.length;

  // 拖入普通链接或选中文本：types 不含 Files。只吞掉默认导航，不猜测目录语义。
  const dragover = { dataTransfer: { types: ['text/uri-list'], dropEffect: 'none' }, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
  page.doc.dispatch('dragover', dragover);
  assert.equal(dragover.defaultPrevented, true, '会话中应阻止非文件拖放的默认导航');

  const drop = { dataTransfer: { types: ['text/plain'], files: [] }, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; } };
  page.doc.dispatch('drop', drop);
  assert.equal(drop.defaultPrevented, true);
  assert.equal(page.xhr.instances.length, before, '非文件拖放不得发起上传');
  assert.equal(page.doc.getElementById('drop-overlay').hidden, true, '非文件拖放不应显示上传遮罩');
});

test('会话结束后旧上传的迟到回调不影响新会话的并发槽与刷新', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['old.bin']);
  const stale = page.xhr.instances[0];
  assert.ok(stale.sent);

  // 断开后在全新 document 上重新配对进入新会话，避免旧监听器污染新界面。
  page.doc.getElementById('disconnect-btn').click();
  await settle();
  const newDoc = await bootFreshPair(page, '#Zz9yY8xX7wW6vV5u', 'fresh-token');

  const listCount = listRequests(page.requests).length;
  // 旧请求在新会话里迟到成功：不得刷新目录、不得改变上传列表。
  stale.respond(200, { name: 'old.bin', size: 4096 });
  await settle();
  assert.equal(listRequests(page.requests).length, listCount, '旧会话上传回调不得触发新会话刷新');

  // 新会话的并发槽完整可用：两个文件应同时发出。
  selectFilesOn(newDoc, ['new1.bin', 'new2.bin']);
  await settle();
  const fresh = page.xhr.instances.slice(1);
  assert.equal(fresh.length, 2, '旧会话占位不得阻塞新会话的并发槽');
});

/* ------------------------------------------------------------------ */
/* 回归：会话切换后的汇总、chip、下载票据归属                            */
/* ------------------------------------------------------------------ */

test('结束会话后顶部上传汇总一并清空，重新配对不残留上一会话统计', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['a.bin', 'b.bin']);
  page.xhr.instances[0].respond(200, { name: 'a.bin', size: 4096 });
  page.xhr.instances[1].respond(200, { name: 'b.bin', size: 4096 });
  await settle();

  assert.match(textOf(page.doc, 'upload-summary'), /2 个文件/, '应有本会话的上传统计');
  assert.match(textOf(page.doc, 'upload-summary'), /已提交 2/);

  page.doc.getElementById('disconnect-btn').click();
  await settle();
  assert.equal(page.doc.getElementById('upload-summary').hidden, true, '结束会话必须同时清空顶部汇总');
  assert.equal(textOf(page.doc, 'upload-summary'), '');

  // 在全新 document 上重新配对：新会话的工具条不能带着上一会话的统计。
  const newDoc = await bootFreshPair(page, '#Zz9yY8xX7wW6vV5u', 'fresh-token');
  assert.equal(newDoc.getElementById('upload-summary').hidden, true, '重新配对后不得显示上一会话的已提交统计');
  assert.equal(newDoc.getElementById('upload-summary').textContent, '');

  // 新会话从零统计：上传一个文件后只计入这一次。
  selectFilesOn(newDoc, ['c.bin']);
  page.xhr.instances[2].respond(200, { name: 'c.bin', size: 4096 });
  await settle();
  const summary = textOf(newDoc, 'upload-summary');
  assert.match(summary, /1 个文件/);
  assert.match(summary, /已提交 1/);
  assert.doesNotMatch(summary, /已提交 3/, '不得把上一会话的计数带进新会话');
});

test('本地倒计时归零后 chip 用琥珀色待确认，不进入红色已过期态', async () => {
  const page = await enterExpiredClockSession();
  try {
    page.timers.intervals[0].fn();
    await settle();

    const chip = page.doc.getElementById('session-chip');
    assert.match(textOf(page.doc, 'session-chip-text'), /正在确认/, '归零期间文案是正在确认');
    assert.equal(chip.classList.contains('is-expired'), false,
      '服务端未确认前不得给出红色 expired 结论');
    assert.equal(chip.classList.contains('is-expiring'), true, '归零期间沿用琥珀色表达待确认');

    // 服务端确认失效后才会结束会话并隐藏 chip；确认有效则恢复正常倒计时配色。
    sessionRequests(page).pop().resolve(jsonResponse(401, { error: { code: 'session_expired' } }));
    await settle();
    assert.equal(page.doc.getElementById('connect-view').hidden, false);
  } finally {
    restoreClock(page);
  }
});

test('迟到的下载票据成功在断开后不得触发浏览器下载', async () => {
  const page = await enterSessionWithRoot();
  const downloadButton = page.doc.getElementById('entries')
    .findAll((node) => node.tagName === 'BUTTON' && node.textContent === '下载')[0];
  downloadButton.click();
  await settle();
  const ticket = page.requests.filter((item) => item.url.indexOf('/api/download-ticket') === 0)[0];
  assert.ok(ticket, '应已发出票据请求');

  // 票据在途时断开会话：迟到的成功响应属于已失效的授权，不得创建下载链接。
  page.doc.getElementById('disconnect-btn').click();
  await settle();
  ticket.resolve(jsonResponse(200, { url: '/download/t/abc123' }));
  await settle();

  assert.equal(page.doc.body.findAll((node) => node.tagName === 'A').length, 0,
    '断开后迟到的票据成功不得交给浏览器下载');
  assert.equal(textOf(page.doc, 'list-status'), '', '断开后不得在连接页残留下载反馈');
});

test('换会话后迟到的下载票据成功不得在新界面触发下载', async () => {
  const page = await enterSessionWithRoot();
  const downloadButton = page.doc.getElementById('entries')
    .findAll((node) => node.tagName === 'BUTTON' && node.textContent === '下载')[0];
  downloadButton.click();
  await settle();
  const ticket = page.requests.filter((item) => item.url.indexOf('/api/download-ticket') === 0)[0];

  // 不等票据返回就断开并用新密钥重新配对进入新会话。
  page.doc.getElementById('disconnect-btn').click();
  await settle();
  const newDoc = await bootFreshPair(page, '#Zz9yY8xX7wW6vV5u', 'fresh-token');
  assert.equal(newDoc.getElementById('browse-view').hidden, false);

  ticket.resolve(jsonResponse(200, { url: '/download/t/abc123' }));
  await settle();

  // 新界面（新会话）上不得出现下载链接；旧会话的授权不能驱动新页面的下载。
  assert.equal(newDoc.body.findAll((node) => node.tagName === 'A').length, 0,
    '换会话后旧票据响应不得在新界面触发下载');
});

test('断开后迟到的下载票据失败也不得写入状态行', async () => {
  const page = await enterSessionWithRoot();
  const downloadButton = page.doc.getElementById('entries')
    .findAll((node) => node.tagName === 'BUTTON' && node.textContent === '下载')[0];
  downloadButton.click();
  await settle();
  const ticket = page.requests.filter((item) => item.url.indexOf('/api/download-ticket') === 0)[0];

  page.doc.getElementById('disconnect-btn').click();
  await settle();
  ticket.resolve(jsonResponse(500, { error: { code: 'internal' } }));
  await settle();

  assert.equal(textOf(page.doc, 'list-status'), '', '失败反馈同样受会话归属约束');
  assert.equal(page.doc.getElementById('connect-view').hidden, false);
});

/* ------------------------------------------------------------------ */
/* 回归：AbortController 中止与 fetch Promise.reject 的确定性时序        */
/* 说明：以下用例运行在假 DOM / 受控 fetch 上，能证明页面逻辑与时序；    */
/* 不等于真实浏览器验收，真机渲染与网络行为仍需原生验证单独覆盖。        */
/* ------------------------------------------------------------------ */

test('加载中导航会中止旧列表请求，迟到的 AbortError 拒绝不覆盖当前列表也不卡住加载', async () => {
  const page = await enterSessionWithRoot();
  // root 请求在 enterSessionWithRoot 里已完成：成功回调会清空 listController，
  // 因此此刻只有一个控制器，且从未被中止。
  assert.equal(page.aborts.length, 1, '完成的 root 请求应创建过一个控制器');
  assert.equal(page.aborts[0].signal.aborted, false, '已完成的请求不会被中止');

  // 进入 d1 的请求挂起，再点面包屑回 root：d1 的控制器应被中止。
  page.doc.getElementById('entries').findAll((node) => node.className === 'entry-label is-dir')[0].click();
  await settle();
  page.doc.getElementById('breadcrumbs').findAll((node) => node.tagName === 'BUTTON')[0].click();
  await settle();
  assert.equal(page.aborts.length, 3);
  assert.equal(page.aborts[1].signal.aborted, true, '被最新导航替换的 d1 请求应已中止');
  assert.equal(page.aborts[2].signal.aborted, false, '当前 root 请求应仍在途');
  const pending = page.requests[page.requests.length - 1];
  assert.equal(pending.init.signal, page.aborts[2].signal, '当前请求应携带在途控制器的 signal');

  // 被中止的 d1 请求以 AbortError 拒绝（真实浏览器 abort 后 fetch 的拒绝形态）。
  const stale = page.requests.filter((item) => item.url.indexOf('dir=d1') >= 0)[0];
  stale.reject(abortError());
  await settle();

  // 旧请求的拒绝不得把网络错误写进当前界面：当前请求仍在加载，
  // 状态行应保持“加载中”，绝不能因旧请求的 AbortError 变成“网络中断”或给出重试入口。
  assert.match(textOf(page.doc, 'list-status'), /加载中/, '当前请求仍在加载');
  assert.doesNotMatch(textOf(page.doc, 'list-status'), /网络中断/, '旧请求的 AbortError 不得写成网络错误');
  assert.match(textOf(page.doc, 'list-count'), /2 项/, '已确认的列表必须保持原样');
  assert.equal(page.doc.getElementById('list-retry').hidden, true, '被中止请求的拒绝不得给出重试入口');

  // 当前请求正常落地后加载结束，不卡在加载态。
  pending.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  assert.match(textOf(page.doc, 'list-count'), /2 项/);
  assert.equal(page.doc.getElementById('list-card').getAttribute('aria-busy'), null, '加载结束后不得残留 aria-busy');
  assert.equal(textOf(page.doc, 'list-status'), '');
});

test('在途列表请求被 Promise.reject 时按网络错误处理，重试可恢复', async () => {
  const page = await enterSessionWithRoot();
  page.doc.getElementById('browse-banner-action').click();
  await settle();
  const pending = page.requests[page.requests.length - 1];

  pending.reject(new Error('boom'));
  await settle();

  assert.match(textOf(page.doc, 'list-status'), /网络中断/, '真实拒绝应按网络错误呈现');
  assert.equal(page.doc.getElementById('list-retry').hidden, false);
  assert.equal(page.aborts[page.aborts.length - 1].signal.aborted, false, '网络拒绝不是主动中止');

  const before = listRequests(page.requests).length;
  page.doc.getElementById('list-retry').click();
  await settle();
  assert.equal(listRequests(page.requests).length, before + 1, '重试应真的再发请求');
  page.requests[page.requests.length - 1].resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  assert.equal(textOf(page.doc, 'list-status'), '', '恢复后不得残留错误文案');
});

test('配对请求 Promise.reject 进入可重试失联态，反复拒绝后成功仍可进入会话', async () => {
  const page = await bootPage({ paired: false });
  await settle();

  // 第一次配对请求真实拒绝：应显示无法连接并保留重试入口。
  pairRequest(page).reject(new Error('boom'));
  await settle();
  assert.match(textOf(page.doc, 'connect-body'), /无法连接服务/);
  assert.equal(page.doc.getElementById('connect-retry').hidden, false);

  // 重试后再次拒绝：状态保持一致，不丢密钥。
  page.doc.getElementById('connect-retry').click();
  await settle();
  lastPairRequest(page).reject(new Error('boom'));
  await settle();
  assert.match(textOf(page.doc, 'connect-body'), /无法连接服务/);
  assert.equal(page.doc.getElementById('connect-retry').hidden, false);

  // 第三次重试成功：必须能正常进入会话，旧拒绝不能污染新代次。
  page.doc.getElementById('connect-retry').click();
  await settle();
  assert.equal(JSON.parse(lastPairRequest(page).init.body).secret, 'Ab3xY9_qLmN0pQrS', '多次拒绝后重试仍复用原密钥');
  lastPairRequest(page).resolve(jsonResponse(200, { token: 't3', expiresAt: 4102444800 }));
  await settle();
  assert.equal(page.doc.getElementById('browse-view').hidden, false, '拒绝后的成功配对应进入浏览页');
  assert.ok(lastListRequest(page.requests), '进入会话后应请求 root');
});

test('会话校验 Promise.reject 计入失联阈值，冷却期内 tick 不重发，周期检查可再问', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;

    page.timers.intervals[0].fn();
    await settle();
    sessionRequests(page)[before].reject(new Error('boom'));
    await settle();

    assert.equal(sessionRequests(page).length, before + 1, '拒绝后不得在回调里立刻重发');
    for (let i = 0; i < 5; i += 1) page.timers.intervals[0].fn();
    await settle();
    assert.equal(sessionRequests(page).length, before + 1, '冷却期内 tick 不得请求');

    // 周期检查继续问；累计 3 次失败后应显示失联横幅。
    page.timers.intervals[1].fn();
    await settle();
    sessionRequests(page)[before + 1].reject(new Error('boom'));
    await settle();
    assert.equal(page.doc.getElementById('browse-banner').hidden, true, '两次失败尚未达到阈值，不得显示横幅');

    page.timers.intervals[1].fn();
    await settle();
    sessionRequests(page)[before + 2].reject(new Error('boom'));
    await settle();
    assert.equal(page.doc.getElementById('browse-banner').hidden, false, '连续三次拒绝应显示失联横幅');
    assert.match(textOf(page.doc, 'browse-banner-text'), /无法连接服务/);
    assert.equal(page.doc.getElementById('browse-banner-action').hidden, false, '横幅应提供重连入口');
  } finally {
    restoreClock(page);
  }
});

test('断开后旧会话在途校验以 reject 落地，不得污染连接页状态', async () => {
  const page = await enterExpiredClockSession();
  try {
    const before = sessionRequests(page).length;
    page.timers.intervals[0].fn();
    await settle();
    const stale = sessionRequests(page)[before];

    page.doc.getElementById('disconnect-btn').click();
    await settle();
    assert.equal(page.doc.getElementById('connect-view').hidden, false);
    const bodyBefore = textOf(page.doc, 'connect-body');

    // 断开后旧校验请求才拒绝：不得改写连接页文案或触发状态变化。
    stale.reject(new Error('boom'));
    await settle();
    assert.equal(textOf(page.doc, 'connect-body'), bodyBefore, '旧会话的拒绝不得改写连接页');
    assert.equal(page.doc.getElementById('browse-view').hidden, true);
  } finally {
    restoreClock(page);
  }
});

test('结束会话后在途列表请求以 reject 落地，重新配对的新会话不受影响', async () => {
  const page = await enterSessionWithRoot();
  page.doc.getElementById('browse-banner-action').click();
  await settle();
  const pending = page.requests[page.requests.length - 1];

  page.doc.getElementById('disconnect-btn').click();
  await settle();
  pending.reject(new Error('boom'));
  await settle();
  assert.equal(page.doc.getElementById('connect-view').hidden, false, '旧会话的拒绝不得破坏连接页');

  // 全新 document 重新配对：新会话的列表加载必须完整可用。
  const newDoc = await bootFreshPair(page, '#Zz9yY8xX7wW6vV5u', 'fresh-token');
  const freshList = lastListRequest(page.requests);
  assert.ok(freshList, '新会话应请求 root');
  freshList.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  assert.equal(newDoc.getElementById('browse-view').hidden, false);
  assert.match(textOf(newDoc, 'list-count'), /2 项/, '新会话列表不得被旧会话的迟到拒绝卡住或污染');
  assert.equal(textOf(newDoc, 'list-status'), '');
});

/* ------------------------------------------------------------------ */
/* 回归：待核对记录的“查看目录”操作                                      */
/* ------------------------------------------------------------------ */

/** 在已进入 root 的会话里造一条待核对上传（非法确认体路径最短）。 */
async function enterWithUncertainUpload() {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['mystery.bin']);
  page.xhr.instances[0].respond(200, { ok: true });
  await settle();
  return page;
}

/** 取第一条上传行上的“查看目录”按钮；无行时返回 null（供残留节点场景使用）。 */
function checkFolderButton(page) {
  const rows = uploadNodes(page);
  if (!rows.length) return null;
  return rows[0].findAll((node) => node.tagName === 'BUTTON')
    .find((node) => node.textContent === '查看目录') || null;
}

/**
 * 直接从 upload-list 捞出当前“查看目录”按钮的节点引用。
 * 与 checkFolderButton 的区别：记录被移除后 checkFolderButton 返回 null，
 * 而这里在移除前先抓引用，用于验证残留闭包节点的行为。
 */
function grabCheckFolderRef(page) {
  return checkFolderButton(page);
}

test('查看目录：同目录点击重发刷新请求，记录与状态保持不变', async () => {
  const page = await enterWithUncertainUpload();
  const before = listRequests(page.requests).length;

  const button = checkFolderButton(page);
  assert.ok(button, '待核对记录应提供查看目录入口');
  button.click();
  await settle();

  const after = listRequests(page.requests);
  assert.equal(after.length, before + 1, '同目录查看应重发一次刷新');
  assert.match(after[after.length - 1].url, /dir=root/, '刷新目标必须是该上传的原目录');

  // 记录不因查看而消失或变形：状态仍是待核对，移除入口仍在。
  assert.equal(uploadStatuses(page).length, 1);
  assert.match(uploadStatuses(page)[0], /无法确认结果/);
  assert.equal(checkFolderButton(page).hidden, false);
});

test('查看目录：跨目录上传导航到原目标目录，而不是当前目录', async () => {
  const page = await enterSessionWithRoot();
  // 进入子目录 d1（ROOT_PAYLOAD 里的目录 id）并发起新上传：targetDir 是 d1。
  page.doc.getElementById('entries').findAll((node) => node.className === 'entry-label is-dir')[0].click();
  await settle();
  lastListRequest(page.requests).resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Share' }, { id: 'd1', name: '2026-11' }],
    entries: [{ id: 'f-spec', name: '设计规范.md', kind: 'file', size: 1, modified: null }],
    nextCursor: null
  }));
  await settle();

  selectFiles(page, ['sub.bin']);
  page.xhr.instances[0].respond(200, { ok: true });
  await settle();
  assert.match(uploadStatuses(page)[0], /无法确认结果/);

  // 待核对会触发对 d1 的自动刷新请求：先让它落地，避免干扰后续导航断言。
  lastListRequest(page.requests).resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Share' }, { id: 'd1', name: '2026-11' }],
    entries: [{ id: 'f-spec', name: '设计规范.md', kind: 'file', size: 1, modified: null }],
    nextCursor: null
  }));
  await settle();

  // 回到 root，再点“查看目录”：必须导航回 d1，而不是刷新当前 root。
  page.doc.getElementById('breadcrumbs').findAll((node) => node.tagName === 'BUTTON')[0].click();
  await settle();
  const rootReq = lastListRequest(page.requests);
  assert.match(rootReq.url, /dir=root/, '面包屑点击应发起 root 导航');
  rootReq.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  const before = listRequests(page.requests).length;
  checkFolderButton(page).click();
  await settle();

  const after = listRequests(page.requests);
  assert.equal(after.length, before + 1);
  assert.match(after[after.length - 1].url, /dir=d1/, '查看必须切回上传的原目标目录');

  // 记录仍然完整保留。
  assert.equal(uploadStatuses(page).length, 1);
  assert.match(uploadStatuses(page)[0], /无法确认结果/);
});

test('查看目录后目标访问失败：错误可见，记录保留，不自动删除也不重传', async () => {
  const page = await enterWithUncertainUpload();
  const beforeXhr = page.xhr.instances.length;

  checkFolderButton(page).click();
  await settle();
  // 目标目录请求失败（服务端 404）。
  lastListRequest(page.requests).resolve(jsonResponse(404, { error: { code: 'dir_not_found' } }));
  await settle();

  // 错误走列表状态行（可见），不是静默。
  assert.match(textOf(page.doc, 'list-status'), /目录不存在/, '目标访问失败必须可见');
  assert.equal(page.doc.getElementById('list-retry').hidden, false, '失败后应有重试入口');

  // 记录原样保留：不因查看失败被删，也没有发出任何重传 XHR。
  assert.equal(uploadStatuses(page).length, 1, '记录不得被自动删除');
  assert.match(uploadStatuses(page)[0], /无法确认结果/);
  assert.equal(page.xhr.instances.length, beforeXhr, '不得自动重传');
});

test('查看目录被目标 200 响应核对后：仍不推断成功，记录等待用户自行处理', async () => {
  const page = await enterWithUncertainUpload();
  checkFolderButton(page).click();
  await settle();
  // 目录正常返回，且里面恰好有一个同名文件——同名不等于就是这条上传的结果。
  lastListRequest(page.requests).resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Share' }],
    entries: [{ id: 'f-m', name: 'mystery.bin', kind: 'file', size: 4096, modified: 1760000000 }],
    nextCursor: null
  }));
  await settle();

  // 绝不按同名推断成功：记录不得变成“已提交”，也不得被自动移除。
  assert.equal(uploadStatuses(page).length, 1);
  assert.match(uploadStatuses(page)[0], /无法确认结果/, '同名文件不得被推断为已提交');
  assert.ok(checkFolderButton(page), '查看入口仍在，用户可再次核对');
});

/**
 * 证据边界：本用例验证的是“断开清理后旧文档的残留节点不再导航”。
 * 真实产品没有“同一闭包内断开再获得新令牌”的入口：重新配对必然整页重新 boot，
 * 生成新的闭包与 state；bootFreshPair 正是这一行为。因此旧按钮闭包看到的是已清空
 * token、已清空上传集合、已递增代次的旧 state，这里能证明的是这些清理生效，
 * 不能单独证明“同一闭包拿到新令牌时由代次/集合守卫拦截”——那条路径无法经真实入口
 * 构造，只由 openUploadTargetDir 的三重校验代码与注释保证。
 */
test('断开并重新配对后，旧文档残留的查看目录节点不得再发起任何导航', async () => {
  const page = await enterWithUncertainUpload();

  // 在旧会话销毁前抓按钮引用：endSession 会把行从 DOM 摘除，
  // 但闭包（按钮监听器与 item 引用）依然活着——残留节点是真实可能被点击的泄漏面。
  const staleButton = grabCheckFolderRef(page);
  assert.ok(staleButton);

  // 断开（清空 token、上传集合，递增代次）后在新 document 上重新配对。
  page.doc.getElementById('disconnect-btn').click();
  await settle();
  const newDoc = await bootFreshPair(page, '#Zz9yY8xX7wW6vV5u', 'fresh-token');
  // 新会话的 root 列表请求在途：先落地，让界面进入稳定态再核对守卫行为。
  lastListRequest(page.requests).resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  const newListCount = listRequests(page.requests).length;

  // 旧界面的残留节点此刻才被点击：旧闭包令牌已清空，不得发出任何列表请求。
  staleButton.click();
  await settle();

  assert.equal(listRequests(page.requests).length, newListCount,
    '断开后的残留查看节点不得发起列表请求');
  // 新会话界面不受影响。
  assert.equal(newDoc.getElementById('browse-view').hidden, false);
  assert.equal(textOf(newDoc, 'list-status'), '');
});

test('记录被用户移除后，残留节点上的查看目录点击不得再发起导航', async () => {
  const page = await enterWithUncertainUpload();

  // 在移除前抓按钮引用：renderUploads 会摘除该行节点，但闭包仍持有 item 与监听器。
  const staleButton = grabCheckFolderRef(page);
  assert.ok(staleButton);

  // 移除记录（合法操作：已结算的待核对记录可移除）。
  const removeButton = uploadNodes(page)[0].findAll((node) => node.tagName === 'BUTTON')
    .find((node) => node.textContent === '移除记录');
  removeButton.click();
  await settle();
  assert.equal(uploadStatuses(page).length, 0, '移除后记录消失');

  // 残留节点引用再点查看目录：记录已不在集合中，必须被归属守卫拒绝。
  const before = listRequests(page.requests).length;
  staleButton.click();
  await settle();
  assert.equal(listRequests(page.requests).length, before,
    '已移除记录的查看点击不得发起任何列表请求');
});

/* ------------------------------------------------------------------ */
/* 回归：未确认进度条语义、面包屑位置标识、断开说明                      */
/* ------------------------------------------------------------------ */

test('结果未确认时进度条不呈现满宽成功外观，辅助技术语义为未知', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['unknown.bin']);
  // 传输走完但确认体非法：满字节已发送，但结果未知。
  page.xhr.instances[0].respond(200, { ok: true });
  await settle();

  const item = uploadNodes(page)[0];
  const bar = item.find((node) => node.classList.contains('upload-bar'));
  assert.ok(bar, '应有进度条');
  assert.equal(bar.getAttribute('role'), 'progressbar', '进度条应对辅助技术暴露进度语义');
  assert.match(bar.getAttribute('aria-valuetext'), /无法确认结果/, '未知结果不能被读成已完成');
  // 不确定进度条按 ARIA 约定省略 aria-valuenow；若同时暴露 100，读屏会先报出“100%”，
  // 与“结果未知”的文字语义冲突。
  assert.equal(bar.getAttribute('aria-valuenow'), null, '未确认时不得暴露百分比数值');
  assert.ok(item.classList.contains('is-uncertain-bar'), '未确认时进度条应切换为不确定外观');
  assert.ok(bar.classList.contains('is-indeterminate'));
  assert.doesNotMatch(bar.getAttribute('aria-valuetext'), /已提交/);
});

test('待核对恢复为已提交后重新暴露数值进度，并移除不确定外观', async () => {
  const page = await enterSessionWithRoot();
  // abort 抛错说明请求可能仍在途：此时界面先转待核对（无数值进度）。
  page.win.XMLHttpRequest = class extends page.xhr.FakeXhr {
    abort() {
      throw new Error('cannot abort');
    }
  };
  selectFiles(page, ['late.bin']);
  const request = page.xhr.instances[0];
  const cancelButton = uploadNodes(page)[0].find((node) => node.tagName === 'BUTTON' && node.textContent === '取消');
  cancelButton.click();
  await settle();

  const item = uploadNodes(page)[0];
  const bar = item.find((node) => node.classList.contains('upload-bar'));
  assert.equal(bar.getAttribute('aria-valuenow'), null, '待核对时不应有数值进度');
  assert.ok(item.classList.contains('is-uncertain-bar'));

  // 请求其实仍送达并成功：同一节点回到已提交终态，数值语义必须恢复。
  request.respond(200, { name: 'late.bin', size: 4096 });
  await settle();

  assert.equal(bar.getAttribute('aria-valuenow'), '100', '恢复已提交后应重新暴露 100');
  assert.equal(bar.getAttribute('aria-valuetext'), '已提交');
  assert.equal(item.classList.contains('is-uncertain-bar'), false, '已提交不得残留不确定外观');
  assert.equal(bar.classList.contains('is-indeterminate'), false);
});

/* ------------------------------------------------------------------ */
/* 回归：同页 hashchange（新分享链接）的浏览器导航语义                    */
/* 真实 Chromium 中同源同路径换 fragment 不重载页面：这里用同一 win 触发 */
/* hashchange，验证同一闭包内的守卫，而不是新 __boot 的新闭包。           */
/* ------------------------------------------------------------------ */

test('断开后同页换新链接：片段立即清除，连接页提供“使用此链接”完成配对', async () => {
  const page = await enterSessionWithRoot();
  page.doc.getElementById('disconnect-btn').click();
  await settle();

  await changeHash(page, '#SecondPairSecret01');
  // 片段必须立即离开地址栏：密钥只允许在内存中停留。
  assert.equal(page.win.location.hash, '', '新链接不得残留在地址栏');
  assert.equal(page.historyCalls.length >= 1, true, '应改写历史清掉片段');
  // 连接页进入 deferred：等待用户显式确认，而不是静默配对。
  assert.match(textOf(page.doc, 'connect-body'), /使用此链接/);
  const pairsBefore = pairRequests(page).length;
  assert.equal(pairsBefore, 1, '只有启动时那一次配对，deferred 不得自动再发');

  // 用户显式确认后才配对。
  page.doc.getElementById('connect-retry').click();
  await settle();
  const pairs = pairRequests(page);
  assert.equal(pairs.length, pairsBefore + 1, '确认后应发出配对请求');
  const body = JSON.parse(pairs[pairs.length - 1].init.body);
  assert.equal(body.secret, 'SecondPairSecret01', '必须使用新链接的密钥');
  pairs[pairs.length - 1].resolve(jsonResponse(200, { token: 'token-second', expiresAt: 4102444800 }));
  await settle();
  assert.equal(page.doc.getElementById('browse-view').hidden, false, '新会话应进入浏览页');
  const listReq = lastListRequest(page.requests);
  assert.ok(listReq, '新会话应请求列表');
  listReq.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  // 进入新会话后暂存密钥与提示都必须清干净。
  assert.equal(page.doc.getElementById('link-notice').hidden, true);
});

test('活跃会话中同页换新链接：不自动切换，上传记录与在途请求全部保留', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['busy.bin']);
  const uploadRequest = page.xhr.instances[0];
  await settle();
  const listCount = listRequests(page.requests).length;

  await changeHash(page, '#SecondPairSecret01');
  // 片段同样立即清除；会话不被打断。
  assert.equal(page.win.location.hash, '', '活跃会话下新链接也不能留在地址栏');
  assert.equal(page.doc.getElementById('browse-view').hidden, false, '活跃会话不得被 fragment 变化打断');
  assert.equal(pairRequests(page).length, 1, '不得用新密钥自动配对（只有启动时那一次）');
  assert.match(textOf(page.doc, 'link-notice-text'), /先断开连接/);
  assert.match(textOf(page.doc, 'link-notice-dismiss'), /知道了/);

  // 在途上传原样保留：回调仍落在原节点上，不被丢成静默。
  uploadRequest.respond(200, { name: 'busy.bin', size: 4096 });
  await settle();
  assert.match(uploadStatuses(page)[0], /已提交/, '在途上传必须正常结算');
  assert.equal(listRequests(page.requests).length > listCount, true, '上传后的目录刷新仍应发生');
  const refresh = lastListRequest(page.requests);
  refresh.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();

  // 提示可手动关闭；关闭后暂存密钥仍在，断开时会再次以 deferred 呈现。
  page.doc.getElementById('link-notice-dismiss').click();
  await settle();
  assert.equal(page.doc.getElementById('link-notice').hidden, true, '提示应可关闭');
  assert.equal(pairRequests(page).length, 1, '关闭提示不得触发新配对');

  // 用户断开后：暂存密钥转为 deferred 选项，不静默丢弃。
  page.doc.getElementById('disconnect-btn').click();
  await settle();
  assert.match(textOf(page.doc, 'connect-body'), /使用此链接/, '断开后应呈现新链接选项');
  assert.equal(pairRequests(page).length, 1, '断开本身不得自动用新密钥配对');
});

test('配对在途时同页换新链接：不打断旧配对，也不静默消费新密钥', async () => {
  const page = await bootPage({ paired: false });
  await settle();
  const firstPair = pairRequest(page);
  assert.ok(firstPair, '启动应发出首次配对');

  await changeHash(page, '#SecondPairSecret01');
  assert.equal(page.win.location.hash, '');
  // 旧配对未被取消：响应仍会被消费。
  assert.equal(pairRequests(page).length, 1, '在途配对不得被打断');
  assert.match(textOf(page.doc, 'link-notice-text'), /等本次连接结束/, '应提示等待当前配对结束');
  assert.equal(pairRequests(page).length, 1, '不得并发发出第二个配对');

  // 旧配对成功落地：新会话正常进入，暂存密钥清空，不再发第二个配对。
  firstPair.resolve(jsonResponse(200, { token: 'token-first', expiresAt: 4102444800 }));
  await settle();
  const listReq = lastListRequest(page.requests);
  assert.ok(listReq, '首个会话应正常建立');
  listReq.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  assert.equal(page.doc.getElementById('browse-view').hidden, false);
  assert.equal(pairRequests(page).length, 1, '新密钥不得在会话建立后自动消费');
});

test('失效片段同页出现：清除片段并提示，不当作密钥发出请求', async () => {
  const page = await enterSessionWithRoot();
  page.doc.getElementById('disconnect-btn').click();
  await settle();

  const requestsBefore = page.requests.length;
  await changeHash(page, '#bad%20secret');
  assert.equal(page.win.location.hash, '', '失效片段同样要离开地址栏');
  assert.equal(page.requests.length, requestsBefore, '失效片段不得发出任何请求');
  assert.match(textOf(page.doc, 'link-notice-text'), /不是有效的分享链接/);

  // 关闭提示后连接页回到普通断开文案；不得残留“使用此链接”。
  page.doc.getElementById('link-notice-dismiss').click();
  await settle();
  assert.equal(page.doc.getElementById('link-notice').hidden, true);
  assert.doesNotMatch(textOf(page.doc, 'connect-body'), /使用此链接/);
});

test('同页换入的新密钥在语言切换后提示与按钮跟随更新', async () => {
  const page = await enterSessionWithRoot();
  page.doc.getElementById('disconnect-btn').click();
  await settle();

  await changeHash(page, '#SecondPairSecret01');
  assert.match(textOf(page.doc, 'connect-body'), /使用此链接/);

  page.doc.getElementById('lang-btn').click();
  await settle();
  assert.match(textOf(page.doc, 'connect-body'), /new share link/i, '切英文后 deferred 文案应同步');
  assert.equal(page.doc.getElementById('connect-retry').textContent, 'Use this link', '按钮文案也应同步');
  // 英文下同样能完成配对闭环。
  page.doc.getElementById('connect-retry').click();
  await settle();
  const pairs = pairRequests(page);
  pairs[pairs.length - 1].resolve(jsonResponse(200, { token: 'token-en', expiresAt: 4102444800 }));
  await settle();
  const listReq = lastListRequest(page.requests);
  listReq.resolve(jsonResponse(200, ROOT_PAYLOAD));
  await settle();
  assert.equal(page.doc.getElementById('browse-view').hidden, false);
});

test('已提交是终态：进度条满宽且语义为已提交，与未确认明确区分', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['good.txt']);
  page.xhr.instances[0].respond(200, { name: 'good.txt', size: 4096 });
  await settle();

  const item = uploadNodes(page)[0];
  const bar = item.find((node) => node.classList.contains('upload-bar'));
  const fill = item.find((node) => node.classList.contains('upload-bar-fill'));
  assert.equal(bar.getAttribute('aria-valuetext'), '已提交');
  assert.equal(bar.getAttribute('aria-valuenow'), '100');
  assert.equal(fill.style.width, '100%');
  assert.equal(item.classList.contains('is-uncertain-bar'), false, '已提交不得带未确认外观');
  assert.ok(item.classList.contains('is-done'));
});

test('传输中保留真实进度百分比，不被未确认语义覆盖', async () => {
  const page = await enterSessionWithRoot();
  selectFiles(page, ['half.bin']);
  const request = page.xhr.instances[0];
  request.upload.onprogress({ lengthComputable: true, loaded: 2048, total: 4096 });
  await settle();

  const item = uploadNodes(page)[0];
  const bar = item.find((node) => node.classList.contains('upload-bar'));
  assert.equal(bar.getAttribute('aria-valuenow'), '50', '传输中应暴露真实进度');
  assert.equal(bar.getAttribute('aria-valuetext'), null, '传输中不需要未知文案');
  assert.equal(item.classList.contains('is-uncertain-bar'), false);
});

test('面包屑有可见的“当前位置”标识，切英文后同步更新', async () => {
  const page = await enterSessionWithRoot();
  const crumbs = page.doc.getElementById('breadcrumbs');
  const label = crumbs.find((node) => node.classList.contains('crumbs-label'));
  assert.ok(label, '面包屑应有可见位置标识');
  assert.equal(label.textContent, '当前位置');
  assert.equal(crumbs.getAttribute('aria-label'), '当前位置');

  page.doc.getElementById('lang-btn').click();
  await settle();
  const englishLabel = page.doc.getElementById('breadcrumbs')
    .find((node) => node.classList.contains('crumbs-label'));
  assert.equal(englishLabel.textContent, 'Current location');
  assert.equal(page.doc.getElementById('breadcrumbs').getAttribute('aria-label'), 'Current location');
});

test('断开按钮明确连接语义与无害性，但不使用危险样式', async () => {
  const page = await enterSessionWithRoot();
  const btn = page.doc.getElementById('disconnect-btn');
  assert.equal(btn.textContent, '断开连接');
  assert.match(btn.getAttribute('title'), /不会删除任何文件/);
  assert.match(btn.getAttribute('aria-label'), /断开连接/);
  // 危险样式约定由 index.html 的静态类名决定（假 DOM 不解析 HTML 的 class 属性），
  // 这里直接核对源码中的按钮声明，确保仍是 ghost 而非 danger/destructive。
  const html = require('node:fs').readFileSync(
    path.join(__dirname, '..', '..', 'web', 'index.html'), 'utf8');
  const tag = html.split('\n').find((line) => line.includes('id="disconnect-btn"'));
  assert.ok(tag, '应能找到断开按钮声明');
  assert.match(tag, /btn-ghost/, '断开应是 ghost 按钮');
  assert.doesNotMatch(tag, /danger|destructive|delete/, '断开不得使用危险/删除样式');
});

test('常驻明文/信任提示与页脚已移除，真实交互提示仍保留', () => {
  const html = require('node:fs').readFileSync(
    path.join(__dirname, '..', '..', 'web', 'index.html'), 'utf8');
  // 用户明确不需要常驻小提示：顶部明文条、标题下说明与页脚都必须彻底移除，
  // 不能只靠 hidden 藏起来，否则仍会占位或被脚本重新显示。
  assert.doesNotMatch(html, /plaintext-notice|plaintext-text/);
  assert.doesNotMatch(html, /id="share-sub"|class="share-sub"/);
  assert.doesNotMatch(html, /<footer|class="foot"|footNote/);
  assert.doesNotMatch(html, /HTTP 明文传输/);
  // 真实交互反馈必须保留：新链接冲突、断线重连、拖拽遮罩。
  assert.match(html, /id="link-notice"/);
  assert.match(html, /id="browse-banner"/);
  assert.match(html, /id="drop-overlay"/);
  // 卡片导航栏内置“上一级”与“刷新”入口。
  assert.match(html, /id="up-btn"/);
  assert.match(html, /id="refresh-btn"/);
});

test('进入子目录后可通过“上一级”按钮与 Alt+ArrowUp 返回父目录，根目录时按钮禁用', async () => {
  const page = await enterSessionWithRoot();
  const upBtn = page.doc.getElementById('up-btn');
  assert.equal(upBtn.disabled, true, '位于根目录时上一级按钮必须禁用');

  // 点击目录行进入子目录
  page.doc.getElementById('entries').findAll((node) => node.className === 'entry-label is-dir')[0].click();
  await settle();
  const enterReq = lastListRequest(page.requests);
  assert.match(enterReq.url, /dir=d1/);
  enterReq.resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Downloads' }, { id: 'd1', name: 'velotype' }],
    entries: [{ id: 'f2', name: 'README.md', kind: 'file', size: 120, modified: 1700000000 }],
    nextCursor: null
  }));
  await settle();

  assert.equal(page.doc.getElementById('share-title').textContent, 'Downloads');
  assert.equal(upBtn.disabled, false, '进入子目录后上一级按钮必须可用');
  assert.match(upBtn.getAttribute('title'), /Downloads/);

  // 点击“上一级”返回根目录
  upBtn.click();
  await settle();
  const upReq = lastListRequest(page.requests);
  assert.match(upReq.url, /dir=root/);
  upReq.resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Downloads' }],
    entries: [{ id: 'd1', name: 'velotype', kind: 'directory', size: null, modified: null }],
    nextCursor: null
  }));
  await settle();
  assert.equal(upBtn.disabled, true, '回到根目录后上一级按钮重新禁用');

  // 再次进入子目录，验证 Alt+ArrowUp 快捷键同样返回父目录
  page.doc.getElementById('entries').findAll((node) => node.className === 'entry-label is-dir')[0].click();
  await settle();
  lastListRequest(page.requests).resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: 'Downloads' }, { id: 'd1', name: 'velotype' }],
    entries: [],
    nextCursor: null
  }));
  await settle();

  let prevented = false;
  page.doc.dispatch('keydown', {
    key: 'ArrowUp',
    altKey: true,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    preventDefault() { prevented = true; }
  });
  await settle();
  assert.equal(prevented, true);
  assert.match(lastListRequest(page.requests).url, /dir=root/);
});

test('服务端根名称退化为 / 时标题与面包屑绝不显示孤立斜杠，文件行不带目录点击态', async () => {
  const page = await bootPage();
  lastListRequest(page.requests).resolve(jsonResponse(200, {
    breadcrumbs: [{ id: 'root', name: '/' }],
    entries: [{ id: 'f1', name: 'notes.txt', kind: 'file', size: 42, modified: 1700000000 }],
    nextCursor: null
  }));
  await settle();

  assert.equal(page.doc.getElementById('share-title').textContent, '共享目录');
  assert.deepEqual(breadcrumbLabels(page.doc), ['共享根目录']);
  const fileLabel = page.doc.getElementById('entries')
    .find((node) => node.className === 'entry-label is-file');
  assert.ok(fileLabel, '文件名称应标记为 is-file 以避免误导性链接悬停样式');
});

test('点击刷新按钮重拉当前目录', async () => {
  const page = await enterSessionWithRoot();
  const countBefore = listRequests(page.requests).length;
  page.doc.getElementById('refresh-btn').click();
  await settle();
  assert.equal(listRequests(page.requests).length, countBefore + 1);
  assert.match(lastListRequest(page.requests).url, /\/api\/list\?dir=root/);
});

