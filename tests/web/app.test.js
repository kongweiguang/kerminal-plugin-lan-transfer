// @author kongweiguang
/*
 * 纯逻辑单元测试（Node 内置 test runner，无第三方依赖）。
 *
 * 之所以只测纯函数、不启动 Sidecar 或浏览器：页面与 Sidecar 的 HTTP 契约由服务端测试覆盖，
 * 这里锁定的是页面自己会算错的部分——片段解析、URL 编码、列表归一化、时间/大小格式、
 * 票据 URL 校验和错误码收敛。真实浏览器联调按任务单在后续统一执行并单独记录。
 */
'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');

const app = require(path.join(__dirname, '..', '..', 'web', 'app.js'));

test('parsePairingHash 接受高熵密钥并去掉井号', () => {
  const result = app.parsePairingHash('#Ab3xY9_qLmN0pQrS');
  assert.equal(result.ok, true);
  assert.equal(result.secret, 'Ab3xY9_qLmN0pQrS');
});

test('parsePairingHash 支持百分号编码但不接受空白与控制字符', () => {
  assert.deepEqual(app.parsePairingHash('#abc%2Bdef%2Fghi'), { ok: true, secret: 'abc+def/ghi' });
  assert.equal(app.parsePairingHash('#abc%20def').ok, false);
  assert.equal(app.parsePairingHash('#abc\u0000def').ok, false);
  assert.equal(app.parsePairingHash('#short').ok, false);
  assert.equal(app.parsePairingHash('').ok, false);
  assert.equal(app.parsePairingHash(null).ok, false);
});

test('parsePairingHash 拒绝超长片段', () => {
  const long = 'a'.repeat(2048);
  assert.equal(app.parsePairingHash('#' + long).ok, false);
});

test('buildListUrl 默认 root 并编码不透明 id 与游标', () => {
  assert.equal(app.buildListUrl('', ''), '/api/list?dir=root');
  assert.equal(app.buildListUrl(null, null), '/api/list?dir=root');
  const url = app.buildListUrl('a b/c&d', 'x+y=1');
  const params = new URLSearchParams(url.split('?')[1]);
  assert.equal(params.get('dir'), 'a b/c&d');
  assert.equal(params.get('cursor'), 'x+y=1');
});

test('buildUploadUrl 保留 UTF-8 文件名与目录', () => {
  const url = app.buildUploadUrl('root', '报告 (1).pdf');
  const params = new URLSearchParams(url.split('?')[1]);
  assert.equal(params.get('dir'), 'root');
  assert.equal(params.get('name'), '报告 (1).pdf');
  assert.ok(url.startsWith('/api/upload?'));
});

test('parseListPayload 接受契约内的列表响应', () => {
  const result = app.parseListPayload({
    breadcrumbs: [{ id: 'root', name: 'Share' }, { id: 'd1', name: '2026-11' }],
    entries: [
      { id: 'a', name: 'a.txt', kind: 'file', size: 12, modified: 1700000000 },
      { id: 'b', name: 'dir', kind: 'directory', size: null, modified: null }
    ],
    nextCursor: 'next'
  });
  assert.equal(result.ok, true);
  assert.deepEqual(result.breadcrumbs, [{ id: 'root', name: 'Share' }, { id: 'd1', name: '2026-11' }]);
  assert.deepEqual(result.entries, [
    { id: 'a', name: 'a.txt', kind: 'file', size: 12, modified: 1700000000 },
    { id: 'b', name: 'dir', kind: 'directory', size: null, modified: null }
  ]);
  assert.equal(result.nextCursor, 'next');
});

test('parseListPayload 把空 body、非 JSON 与缺字段判为协议错误而不是空目录', () => {
  for (const payload of [null, undefined, '', 'x', 0, [], {}, { entries: [] }, { breadcrumbs: [] },
    { breadcrumbs: [{ id: 'root', name: 'Share' }] }]) {
    const result = app.parseListPayload(payload);
    assert.equal(result.ok, false, '应为协议错误: ' + JSON.stringify(payload));
    assert.equal(result.reason, 'body');
  }
});

test('parseListPayload 拒绝未知 kind、非法 id 与非法 nextCursor', () => {
  const base = { breadcrumbs: [{ id: 'root', name: 'Share' }], entries: [], nextCursor: null };
  const cases = [
    { ...base, entries: [{ id: 'c', name: 'link', kind: 'symlink', size: 1, modified: 1 }] },
    { ...base, entries: [{ id: '', name: 'noid', kind: 'file' }] },
    { ...base, entries: [{ id: 'd', name: 42, kind: 'file' }] },
    { ...base, entries: [{ id: 'e', name: 'big', kind: 'file', size: -1, modified: null }] },
    { ...base, entries: [{ id: 'f', name: 'bad', kind: 'file', size: null, modified: 'not-a-date' }] },
    { ...base, entries: [{ id: 'g', name: 'bool', kind: 'file', size: true, modified: null }] },
    { ...base, nextCursor: 7 },
    { ...base, breadcrumbs: [{ id: 'other', name: 'NotRoot' }] },
    { ...base, breadcrumbs: [{ id: '', name: 'bad' }] }
  ];
  for (const payload of cases) {
    assert.equal(app.parseListPayload(payload).ok, false, '应拒绝: ' + JSON.stringify(payload));
  }
});

test('parseListPayload 把 nextCursor 的 null/空串归一化为 null，缺失则判协议错误', () => {
  const base = { breadcrumbs: [{ id: 'root', name: 'Share' }], entries: [] };
  assert.equal(app.parseListPayload({ ...base, nextCursor: null }).nextCursor, null);
  assert.equal(app.parseListPayload({ ...base, nextCursor: '' }).nextCursor, null);
  // 缺失 nextCursor 时无法区分“最后一页”和“服务端漏字段”，必须显式报错而不是静默截断分页。
  assert.equal(app.parseListPayload({ ...base }).ok, false);
  assert.equal(app.parseListPayload({ ...base }).reason, 'cursor');
});

test('listTargetMatches 要求最后面包屑与请求目录一致，翻页也一样', () => {
  const crumbs = [{ id: 'root', name: 'Share' }, { id: 'd1', name: '2026-11' }];
  assert.equal(app.listTargetMatches(crumbs, 'd1'), true);
  assert.equal(app.listTargetMatches(crumbs, 'root'), false, '目录错位必须判为不匹配');
  assert.equal(app.listTargetMatches([{ id: 'root', name: 'Share' }], 'root'), true);
  assert.equal(app.listTargetMatches([{ id: 'root', name: 'Share' }], ''), true, '空目录按 root 处理');
  assert.equal(app.listTargetMatches([], 'root'), false);
  assert.equal(app.listTargetMatches(null, 'root'), false);
});

test('mergeEntries 按 id 去重且保留已有顺序', () => {
  const merged = app.mergeEntries(
    [{ id: 'a', name: 'a' }, { id: 'b', name: 'b' }],
    [{ id: 'b', name: 'b2' }, { id: 'c', name: 'c' }]
  );
  assert.deepEqual(merged.map((item) => item.id), ['a', 'b', 'c']);
  assert.equal(merged[1].name, 'b');
});

test('formatSize 使用 1024 进制并对未知值显示破折号', () => {
  assert.equal(app.formatSize(0), '0 B');
  assert.equal(app.formatSize(1023), '1023 B');
  assert.equal(app.formatSize(1024), '1.0 KB');
  assert.equal(app.formatSize(1536), '1.5 KB');
  assert.equal(app.formatSize(5 * 1024 * 1024), '5.0 MB');
  assert.equal(app.formatSize(null), '—');
  assert.equal(app.formatSize(-5), '—');
  assert.equal(app.formatSize(Number.NaN), '—');
});

test('formatModified 支持 Unix 秒、毫秒与 ISO 字符串', () => {
  assert.equal(app.formatModified(null), '—');
  assert.equal(app.formatModified('nope'), '—');
  assert.match(app.formatModified(1700000000), /^\d{2}-\d{2} \d{2}:\d{2}$/);
  assert.match(app.formatModified(1700000000000), /^\d{2}-\d{2} \d{2}:\d{2}$/);
  assert.equal(app.formatModified(1700000000), app.formatModified(1700000000000));
  assert.match(app.formatModified('2026-11-11T02:00:00Z'), /^\d{2}-\d{2} \d{2}:\d{2}$/);
});

test('formatRemaining 在过期与临界区给出稳定状态', () => {
  const now = 1_700_000_000_000;
  const expiresAt = now / 1000 + 90;
  const result = app.formatRemaining(expiresAt, now);
  assert.equal(result.expired, false);
  assert.equal(result.expiring, true);
  assert.equal(result.text, '01:30');

  const hour = app.formatRemaining(now / 1000 + 3700, now);
  assert.equal(hour.text, '1:01:40');

  const expired = app.formatRemaining(now / 1000 - 1, now);
  assert.equal(expired.expired, true);
  assert.equal(expired.text, '00:00');

  assert.equal(app.formatRemaining(null, now).expired, true);
});

test('isSafeTicketUrl 只接受带票据段的同源下载地址', () => {
  const origin = 'http://192.168.1.8:53124';
  assert.equal(app.isSafeTicketUrl('/download/t/abc', origin), '/download/t/abc');
  assert.equal(app.isSafeTicketUrl('/api/download/t/abc?x=1', origin), '/api/download/t/abc?x=1');
  assert.equal(app.isSafeTicketUrl('http://192.168.1.8:53124/api/download/t/abc', origin), '/api/download/t/abc');
  assert.equal(app.isSafeTicketUrl('http://192.168.1.9:53124/api/download/t/abc', origin), null);
  assert.equal(app.isSafeTicketUrl('https://192.168.1.8:53124/api/download/t/abc', origin), null);
  assert.equal(app.isSafeTicketUrl('//evil.example/api/download/t/abc', origin), null);
  assert.equal(app.isSafeTicketUrl('http://user:pw@192.168.1.8:53124/api/download/t/abc', origin), null);
  assert.equal(app.isSafeTicketUrl('javascript:alert(1)', origin), null);
  assert.equal(app.isSafeTicketUrl('', origin), null);
  assert.equal(app.isSafeTicketUrl('/api/download/t/abc#frag', origin), '/api/download/t/abc');
  // 同源但不是票据路径：误返回首页、健康检查或空票据时不能交给浏览器下载。
  assert.equal(app.isSafeTicketUrl('/', origin), null);
  assert.equal(app.isSafeTicketUrl('/index.html', origin), null);
  assert.equal(app.isSafeTicketUrl('/api/session', origin), null);
  assert.equal(app.isSafeTicketUrl('/download/', origin), null);
  assert.equal(app.isSafeTicketUrl('/download', origin), null);
});

test('describeApiError 优先使用服务端错误码并收敛为稳定小写码', () => {
  assert.deepEqual(app.describeApiError(403, { error: { code: 'Path_Rejected', message: 'C:\\secret' } }),
    { code: 'path_rejected', status: 403 });
  assert.deepEqual(app.describeApiError(401, null), { code: 'unauthorized', status: 401 });
  assert.deepEqual(app.describeApiError(413, null), { code: 'too_large', status: 413 });
  assert.deepEqual(app.describeApiError(429, null), { code: 'busy', status: 429 });
  assert.deepEqual(app.describeApiError(500, null), { code: 'server', status: 500 });
  assert.deepEqual(app.describeApiError(0, null), { code: 'network', status: 0 });
  assert.deepEqual(app.describeApiError(418, null), { code: 'request', status: 418 });
});

test('validatePairResponse 拒绝缺字段的配对响应', () => {
  assert.deepEqual(app.validatePairResponse({ token: 't', expiresAt: 123 }), { token: 't', expiresAt: 123 });
  assert.equal(app.validatePairResponse({ token: '', expiresAt: 123 }), null);
  assert.equal(app.validatePairResponse({ token: 't' }), null);
  assert.equal(app.validatePairResponse({ token: 't', expiresAt: '123' }), null);
  assert.equal(app.validatePairResponse(null), null);
});

test('themeCycle 按 自动 -> 浅色 -> 深色 循环', () => {
  assert.equal(app.themeCycle('auto'), 'light');
  assert.equal(app.themeCycle('light'), 'dark');
  assert.equal(app.themeCycle('dark'), 'auto');
  assert.equal(app.themeCycle(undefined), 'light');
});

test('localeFromNavigator 只把中文识别为 zh', () => {
  assert.equal(app.localeFromNavigator('zh-CN'), 'zh');
  assert.equal(app.localeFromNavigator('zh-Hant-TW'), 'zh');
  assert.equal(app.localeFromNavigator('en-US'), 'en');
  assert.equal(app.localeFromNavigator('ja-JP'), 'en');
  assert.equal(app.localeFromNavigator(undefined), 'zh');
});

test('translate 支持占位替换并在缺翻译时回退', () => {
  assert.equal(app.translate('zh', 'itemsCount', { count: 3 }), '3 项');
  assert.equal(app.translate('en', 'itemsCount', { count: 3 }), '3 items');
  assert.equal(app.translate('en', 'noSuchKey'), 'noSuchKey');
  assert.equal(app.translate('zh', 'sessionRemaining', { time: '01:00' }), '会话剩余 01:00');
});
