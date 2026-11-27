// @author kongweiguang
/*
 * 浏览器端页面脚本（由 Rust Sidecar 内嵌，无构建步骤）。
 *
 * 设计原因与约束：
 * - 页面与 Sidecar 同源，所有请求都使用相对路径，不向任何第三方发请求；这样既满足 CSP，
 *   也避免配对信息或下载票据经 Referer 泄漏。
 * - 配对密钥只从 URL fragment 读取一次，随即用 replaceState 从地址栏和当前历史条目移除；
 *   令牌只保存在内存里，不写 localStorage/sessionStorage，也不写日志。
 * - 上传用 XMLHttpRequest，是为了拿到逐文件进度、取消能力和“传输完成但尚未确认”的中间态；
 *   下载走短期票据加浏览器原生下载，不把整文件读进内存，也不把“服务端发送完成”说成“浏览器已保存”。
 * - 纯逻辑集中在本文件上半部分并通过 module.exports 暴露给 Node 内置测试；没有 document 时
 *   不会启动任何 DOM 逻辑，因此测试不需要浏览器环境。
 */
(function (globalRoot, factory) {
  'use strict';
  var api = factory();
  if (typeof module === 'object' && module && module.exports) {
    module.exports = api;
  }
  if (globalRoot) {
    globalRoot.LanTransfer = api;
  }
  if (typeof document !== 'undefined' && typeof window !== 'undefined') {
    api.__boot(window, document);
  }
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  var MIN_SECRET_LENGTH = 8;
  var MAX_SECRET_LENGTH = 1024;
  var SESSION_EXPIRING_SECONDS = 300;
  var MAX_CONCURRENT_UPLOADS = 2;
  var SESSION_CHECK_INTERVAL_MS = 30000;
  // 页面恢复可见时用户可能快速来回切标签；给自动校验留最小间隔，
  // 避免每次聚焦都发请求。手动重试与周期检查不受此限制。
  var VISIBILITY_CHECK_GAP_MS = 5000;
  var UNREACHABLE_AFTER_FAILURES = 3;

  /* ------------------------------------------------------------------ */
  /* 纯逻辑：可被 Node 内置测试直接覆盖                                   */
  /* ------------------------------------------------------------------ */

  /**
   * 从 location.hash 提取配对密钥。
   * 之所以在客户端先做长度和字符校验，是为了避免把空片段、页面锚点或粘贴残留当成密钥
   * 发给服务端；真正的高熵校验仍由 Sidecar 完成。片段按 URL 规则可能被百分号编码，
   * 因此先尝试解码，解码失败则保留原文（base64url 密钥不受影响）。
   */
  function parsePairingHash(hash) {
    if (typeof hash !== 'string') return { ok: false, reason: 'missing' };
    var value = hash.charAt(0) === '#' ? hash.slice(1) : hash;
    if (value === '') return { ok: false, reason: 'missing' };
    if (value.length > MAX_SECRET_LENGTH) return { ok: false, reason: 'invalid' };
    if (/[\s\u0000-\u001f\u007f]/.test(value)) return { ok: false, reason: 'invalid' };
    var decoded = value;
    try {
      decoded = decodeURIComponent(value);
    } catch (error) {
      decoded = value;
    }
    if (decoded.length < MIN_SECRET_LENGTH || decoded.length > MAX_SECRET_LENGTH) {
      return { ok: false, reason: 'invalid' };
    }
    if (/[\s\u0000-\u001f\u007f]/.test(decoded)) return { ok: false, reason: 'invalid' };
    return { ok: true, secret: decoded };
  }

  /**
   * 生成目录列表地址。目录 id 和游标都是服务端生成的不透明字符串，
   * 必须用 URLSearchParams 编码，避免文件名或游标中的特殊字符改变查询语义。
   */
  function buildListUrl(dir, cursor) {
    var params = new URLSearchParams();
    params.set('dir', dir == null || dir === '' ? 'root' : String(dir));
    if (cursor != null && cursor !== '') params.set('cursor', String(cursor));
    return '/api/list?' + params.toString();
  }

  /**
   * 生成上传地址。文件名以 UTF-8 百分号编码放进查询串，
   * 与服务端“name 是 UTF-8 文件名”的契约保持一致，同时避免路径分隔符被误解为路径。
   */
  function buildUploadUrl(dir, name) {
    var params = new URLSearchParams();
    params.set('dir', dir == null || dir === '' ? 'root' : String(dir));
    params.set('name', String(name == null ? '' : name));
    return '/api/upload?' + params.toString();
  }

  /**
   * 严格解析 /api/list 的 200 响应。
   *
   * 这里刻意不做“能读多少读多少”的宽容处理：空 body、非 JSON、缺字段、未知 kind、
   * 非法 id 或非法 nextCursor 都必须报协议错误，由调用方保留上一次已确认的列表。
   * 原因是把坏响应归一化成空数组，会让用户看到“目录为空”这种伪成功，进而误以为文件已丢失；
   * 而悄悄丢掉一个未知 kind 的条目，会让下载/打开操作缺失且没有任何提示。
   */
  function parseListPayload(payload) {
    if (!payload || typeof payload !== 'object' || Array.isArray(payload)) {
      return { ok: false, reason: 'body' };
    }
    if (!Array.isArray(payload.breadcrumbs) || !Array.isArray(payload.entries)) {
      return { ok: false, reason: 'body' };
    }

    var breadcrumbs = [];
    for (var i = 0; i < payload.breadcrumbs.length; i += 1) {
      var crumb = payload.breadcrumbs[i];
      if (!crumb || typeof crumb !== 'object' || Array.isArray(crumb)) {
        return { ok: false, reason: 'breadcrumb' };
      }
      if (typeof crumb.id !== 'string' || crumb.id === '') {
        return { ok: false, reason: 'breadcrumb' };
      }
      if (typeof crumb.name !== 'string') {
        return { ok: false, reason: 'breadcrumb' };
      }
      breadcrumbs.push({ id: crumb.id, name: crumb.name });
    }
    // 契约规定共享根 id 固定为 root；首项不是 root 说明响应来自别的实现或已被篡改。
    if (breadcrumbs.length === 0 || breadcrumbs[0].id !== 'root') {
      return { ok: false, reason: 'breadcrumb' };
    }

    var entries = [];
    for (var j = 0; j < payload.entries.length; j += 1) {
      var entry = payload.entries[j];
      if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
        return { ok: false, reason: 'entry' };
      }
      if (typeof entry.id !== 'string' || entry.id === '') {
        return { ok: false, reason: 'entry' };
      }
      if (typeof entry.name !== 'string' || entry.name === '') {
        return { ok: false, reason: 'entry' };
      }
      if (entry.kind !== 'file' && entry.kind !== 'directory') {
        return { ok: false, reason: 'entry' };
      }
      var size = entry.size;
      if (size !== null && !(typeof size === 'number' && isFinite(size) && size >= 0)) {
        return { ok: false, reason: 'entry' };
      }
      // 契约只要求 modified 可为 null，没有限定秒数还是字符串；这里接受可解析的两种形式，
      // 其他类型或不可解析的字符串按协议错误处理，避免把无意义时间静默渲染成 “—”。
      var modified = entry.modified;
      if (modified !== null) {
        if (typeof modified === 'number' && isFinite(modified)) {
          // 合法数值直接保留。
        } else if (typeof modified === 'string' && modified !== '' && !isNaN(new Date(modified).getTime())) {
          // 合法日期字符串保留原样，显示时再统一格式化。
        } else {
          return { ok: false, reason: 'entry' };
        }
      }
      entries.push({ id: entry.id, name: entry.name, kind: entry.kind, size: size, modified: modified });
    }

    // nextCursor 是契约必填字段：缺失时无法区分“没有下一页”和“服务端忘了带游标”，
    // 直接当成没有下一页会让分页静默截断，所以缺失按协议错误处理；只有显式 null 才表示结束。
    if (!Object.prototype.hasOwnProperty.call(payload, 'nextCursor')) {
      return { ok: false, reason: 'cursor' };
    }
    var nextCursor = payload.nextCursor;
    if (nextCursor === null || nextCursor === '') {
      nextCursor = null;
    } else if (typeof nextCursor !== 'string') {
      return { ok: false, reason: 'cursor' };
    }

    return { ok: true, breadcrumbs: breadcrumbs, entries: entries, nextCursor: nextCursor };
  }

  /**
   * 列表响应的最后一级面包屑必须等于本次请求的目录（翻页也一样）。
   * 服务端若返回了别的目录，说明响应与请求错位；此时把它当协议错误处理，
   * 让调用方保留上一次已确认的列表和重试目标，避免把 A 目录的内容显示成 B 目录。
   */
  function listTargetMatches(breadcrumbs, dirId) {
    if (!Array.isArray(breadcrumbs) || breadcrumbs.length === 0) return false;
    var expected = dirId == null || dirId === '' ? 'root' : String(dirId);
    var last = breadcrumbs[breadcrumbs.length - 1];
    return !!last && typeof last.id === 'string' && last.id === expected;
  }

  /**
   * 分页追加时按 id 去重。服务端游标可能因目录变化而重叠，
   * 去重可以避免同一个文件出现两行并让“下载”按钮指向同一目标。
   */
  function mergeEntries(previous, incoming) {
    var merged = Array.isArray(previous) ? previous.slice() : [];
    var seen = Object.create(null);
    for (var i = 0; i < merged.length; i += 1) seen[merged[i].id] = true;
    var next = Array.isArray(incoming) ? incoming : [];
    for (var j = 0; j < next.length; j += 1) {
      if (seen[next[j].id]) continue;
      seen[next[j].id] = true;
      merged.push(next[j]);
    }
    return merged;
  }

  /**
   * 文件大小只做显示用途，统一用 1024 进制并保留一位小数，
   * 与服务端返回的字节数保持同一含义；未知大小显示破折号而不是 0。
   */
  function formatSize(bytes) {
    if (typeof bytes !== 'number' || !isFinite(bytes) || bytes < 0) return '—';
    if (bytes < 1024) return Math.floor(bytes) + ' B';
    var units = ['KB', 'MB', 'GB', 'TB', 'PB'];
    var value = bytes / 1024;
    var index = 0;
    while (value >= 1024 && index < units.length - 1) {
      value /= 1024;
      index += 1;
    }
    var digits = value >= 100 ? 0 : 1;
    return value.toFixed(digits) + ' ' + units[index];
  }

  /**
   * 解析修改时间。契约允许 null、Unix 秒或 ISO 字符串；
   * 这里把大数值按毫秒处理，避免把毫秒时间戳误读成 1970 年。
   */
  function parseModified(value) {
    if (typeof value === 'number' && isFinite(value)) {
      var millis = value > 1e11 ? value : value * 1000;
      var fromNumber = new Date(millis);
      return isNaN(fromNumber.getTime()) ? null : fromNumber;
    }
    if (typeof value === 'string' && value !== '') {
      var fromString = new Date(value);
      return isNaN(fromString.getTime()) ? null : fromString;
    }
    return null;
  }

  /**
   * 固定两位补零。倒计时和日期列都要求等宽数字，避免秒数变化时文本宽度跳动。
   */
  function pad2(value) {
    return value < 10 ? '0' + value : String(value);
  }

  /**
   * 修改时间用固定的 MM-DD HH:mm 本地格式，避免不同浏览器 locale 造成列宽跳动；
   * 时间无效时显示破折号，不显示 “Invalid Date”。
   */
  function formatModified(value) {
    var date = parseModified(value);
    if (!date) return '—';
    return pad2(date.getMonth() + 1) + '-' + pad2(date.getDate()) + ' ' +
      pad2(date.getHours()) + ':' + pad2(date.getMinutes());
  }

  /**
   * 计算会话剩余时间。expiresAt 是 Unix 秒，剩余不足 5 分钟时标记为即将过期，
   * 让界面可以提前变色而不是等到失效才提示；倒计时按整秒向下取整，避免显示 0 秒却仍可用。
   */
  function formatRemaining(expiresAt, nowMs) {
    if (typeof expiresAt !== 'number' || !isFinite(expiresAt)) {
      return { expired: true, expiring: false, text: '00:00' };
    }
    var now = typeof nowMs === 'number' && isFinite(nowMs) ? nowMs : Date.now();
    var seconds = Math.floor(expiresAt - now / 1000);
    if (seconds <= 0) return { expired: true, expiring: false, text: '00:00' };
    var hours = Math.floor(seconds / 3600);
    var minutes = Math.floor((seconds % 3600) / 60);
    var rest = seconds % 60;
    var text = hours > 0
      ? hours + ':' + pad2(minutes) + ':' + pad2(rest)
      : pad2(minutes) + ':' + pad2(rest);
    return { expired: false, expiring: seconds <= SESSION_EXPIRING_SECONDS, text: text };
  }

  /**
   * 校验下载票据 URL。票据只能用于同源下载，因此拒绝跨源、带 userinfo、非 HTTP(S)
   * 以及协议相对地址；返回值去掉 fragment 后作为相对路径交给浏览器，
   * 避免票据出现在 Referer 或被带到其他主机。
   *
   * 除了同源，还要求路径里确实存在一段非空的 `/download/<ticket>`：
   * 服务端如果误返回首页、健康检查或空票据地址，浏览器会下载到一个 HTML 页面或直接失败，
   * 与其把这种地址交给浏览器，不如在页面内直接判为票据无效。
   */
  function isSafeTicketUrl(url, origin) {
    if (typeof url !== 'string' || url === '') return null;
    var base;
    try {
      base = new URL(origin);
    } catch (error) {
      return null;
    }
    var target;
    try {
      target = new URL(url, base);
    } catch (error) {
      return null;
    }
    if (target.protocol !== 'http:' && target.protocol !== 'https:') return null;
    if (target.origin !== base.origin) return null;
    if (target.username || target.password) return null;
    if (target.pathname.charAt(0) !== '/') return null;
    if (target.pathname.indexOf('//') === 0) return null;
    if (!hasDownloadTicketSegment(target.pathname)) return null;
    return target.pathname + target.search;
  }

  /**
   * 路径中必须出现 `download/<非空票据>`。允许 `/download/<t>` 与 `/api/download/<t>`
   * 两种前缀，因为前缀属于服务端路由，而票据段必须紧跟 download 且不能为空。
   */
  function hasDownloadTicketSegment(pathname) {
    var segments = pathname.split('/');
    for (var i = 0; i < segments.length - 1; i += 1) {
      if (segments[i] !== 'download') continue;
      if (segments[i + 1] !== '') return true;
    }
    return false;
  }

  /**
   * 把错误响应收敛为稳定的错误码，界面只按错误码显示本地化文案。
   * 故意不把服务端 message 直接写进界面，避免异常文本里夹带本机绝对路径或令牌。
   */
  function describeApiError(status, payload) {
    var code = '';
    if (payload && typeof payload === 'object' && payload.error && typeof payload.error === 'object' &&
        typeof payload.error.code === 'string') {
      code = payload.error.code.replace(/[^A-Za-z0-9_.-]/g, '').slice(0, 64).toLowerCase();
    }
    var numeric = typeof status === 'number' ? status : 0;
    if (!code) {
      if (numeric === 0) code = 'network';
      else if (numeric === 400) code = 'bad_request';
      else if (numeric === 401) code = 'unauthorized';
      else if (numeric === 403) code = 'forbidden';
      else if (numeric === 404) code = 'not_found';
      else if (numeric === 409) code = 'conflict';
      else if (numeric === 413) code = 'too_large';
      else if (numeric === 429) code = 'busy';
      else if (numeric >= 500) code = 'server';
      else code = 'request';
    }
    return { code: code, status: numeric };
  }

  /**
   * 校验配对响应。token 与 expiresAt 缺一不可，否则宁可让用户重新打开链接，
   * 也不要带着半截会话状态进入浏览页。
   */
  function validatePairResponse(payload) {
    if (!payload || typeof payload !== 'object') return null;
    if (typeof payload.token !== 'string' || payload.token === '') return null;
    if (typeof payload.expiresAt !== 'number' || !isFinite(payload.expiresAt)) return null;
    return { token: payload.token, expiresAt: payload.expiresAt };
  }

  /**
   * 主题三态循环：自动 -> 浅色 -> 深色 -> 自动。
   * 用固定顺序而不是读取系统值，是因为“自动”本身就是用户可回退的选项。
   */
  function themeCycle(current) {
    if (current === 'light') return 'dark';
    if (current === 'dark') return 'auto';
    return 'light';
  }

  /**
   * 语言只按浏览器首选语言做一次初始判断；之后由用户手动切换并记住，
   * 不再跟随系统，避免页面在使用中突然换语言。
   */
  function localeFromNavigator(language) {
    if (typeof language !== 'string') return 'zh';
    return language.toLowerCase().indexOf('zh') === 0 ? 'zh' : 'en';
  }

  var ZH = {
    sessionRemaining: '会话剩余 {time}',
    sessionChecking: '正在确认会话…',
    connectTitle: '连接共享目录',
    connectBodyPairing: '正在与 Kerminal 建立会话…',
    connectBodyMissing: '请回到 Kerminal 的「局域网文件传输」面板，复制分享链接后重新打开。',
    connectBodyRejected: '分享链接无效或已过期，请在 Kerminal 中重新复制链接。',
    connectBodyUnreachable: '暂时无法连接服务，请确认 Kerminal 仍在运行后重试。',
    // 限流说明服务端在线但暂时拒绝请求：与“无法连接”区分开，明确这是可等待重试的暂时状态，
    // 不承诺具体等待时长（服务端的限流窗口页面无从得知）。
    connectBodyRateLimited: '服务暂时限流，请稍等片刻后重试。',
    // 响应格式异常或状态未知时只能说“无法确认”，不能据此断言分享链接已失效。
    connectBodyUnconfirmed: '暂时无法确认分享链接状态，请稍后重试。',
    connectBodyExpired: '会话已失效。请回到 Kerminal 重新复制分享链接。',
    connectBodyDisconnected: '已断开连接。如需继续，请在 Kerminal 中重新复制分享链接。',
    connectBodyDeferred: '当前未连接共享目录。收到的新分享链接仍保留在本页，点“使用此链接”开始配对。',
    // 同页换 fragment 不触发重载（Chromium 实测）。新链接只在断开/空闲时可用：
    // 活跃会话期间不能因为换了一个链接就丢弃在途上传或待核对记录。
    linkNoticeActiveSession: '检测到新的分享链接，但当前会话未结束；请先断开连接，再使用新链接。',
    linkNoticePairing: '正在建立连接，新的分享链接暂未使用；请等本次连接结束后再打开新链接。',
    linkNoticeUse: '使用此链接',
    linkNoticeInvalid: '地址栏出现的新链接不是有效的分享链接，已清除。',
    linkNoticeDismiss: '知道了',
    retry: '重试',
    reconnect: '重新连接',
    // 断开按钮紧邻语言/主题按钮，单写“断开”看不出断开的是什么；写明“断开连接”，
    // 但仍用普通 ghost 样式，避免红色危险样式暗示会删除数据或断开其他程序。
    disconnect: '断开连接',
    disconnectHint: '结束本页与共享目录的连接；不会删除任何文件。',
    // 面包屑栏标题，让只有文件夹名的一行明确表达“当前位置”而不是输入框。
    breadcrumbLabel: '当前位置',
    goUp: '上一级',
    goUpHint: '返回上一级目录 (Alt+↑)',
    goUpTo: '返回上一级：{name} (Alt+↑)',
    refresh: '刷新',
    refreshHint: '刷新当前目录',
    shareTitle: '共享目录',
    colName: '名称',
    colSize: '大小',
    colModified: '修改时间',
    colAction: '操作',
    loadMore: '加载更多',
    loading: '加载中…',
    itemsCount: '{count} 项',
    emptyDir: '此目录为空，可以选择文件上传。',
    open: '打开',
    download: '下载',
    downloading: '获取中…',
    downloadStarted: '已交给浏览器下载；是否保存以浏览器为准。',
    downloadFailed: '下载未开始',
    selectFiles: '选择文件',
    dropHint: '拖放文件即可上传',
    dropHere: '松开以上传到当前目录',
    uploadsTitle: '上传',
    uploadTarget: '上传 · {path}',
    uploadMultipleTargets: '上传 · 多个目录',
    uploadQueued: '排队中',
    uploadInProgress: '上传中…',
    uploadProgress: '{percent}%',
    uploadAwaiting: '传输完成，等待服务器确认…',
    uploadSubmitted: '已提交',
    uploadSubmittedAs: '已提交为 {name}',
    uploadCancelled: '已取消',
    // 在途取消或网络中断时，请求可能已经完整送达并在服务端提交，
    // 因此这里只陈述“无法确定”，不能写成失败，也不能写成已提交。
    // 文案给普通语言的操作指引：无法确认结果，请查看目标目录（配合“查看目录”按钮闭环）。
    uploadUncertain: '无法确认结果，请查看目标目录',
    uploadUncertainHint: '无法确认该文件是否已上传；点击“查看目录”到原目标目录核实，此处不会自动重传。',
    viewTargetDir: '查看目录',
    uploadSummary: '{count} 个文件 · {size} · 已提交 {done}',
    uploadSummaryUncertain: '{count} 个待核对',
    cancel: '取消',
    cancelAll: '全部取消',
    clearDone: '清除已完成',
    // 批量清理只移除已结算且非待核对的记录，避免把未知结果当成已完成静默丢弃。
    clearDoneHint: '清除已完成项；结果待核对的记录会保留，可逐项移除',
    remove: '移除',
    removeRecord: '移除记录',
    removeRecordHint: '仅从本页移除这条待核对记录，不会删除服务器上的文件。',
    themeAuto: '自动',
    themeLight: '浅色',
    themeDark: '深色',
    themeTitle: '切换主题',
    langTitle: '切换语言',
    serviceUnreachable: '无法连接服务，请确认 Kerminal 仍在运行。',
    rootName: '共享根目录',
    'error.unauthorized': '会话已失效，请重新打开分享链接。',
    'error.session_expired': '会话已过期，请重新打开分享链接。',
    'error.invalid_secret': '配对失败，分享链接可能已失效。',
    'error.forbidden': '没有权限执行此操作。',
    'error.not_found': '文件或目录不存在。',
    'error.dir_not_found': '目录不存在或已被移出共享范围。',
    'error.ticket_expired': '下载票据已过期，请重试。',
    'error.too_large': '文件超过大小限制。',
    'error.busy': '服务繁忙，请稍后重试。',
    'error.conflict': '同名文件已存在，且无法自动改名。',
    'error.quota': '存储空间不足。',
    'error.server': '服务内部错误，请稍后重试。',
    'error.internal': '服务内部错误，请稍后重试。',
    'error.unavailable': '服务暂时不可用。',
    'error.bad_request': '请求无效。',
    'error.path_rejected': '路径不被允许。',
    'error.protocol': '服务返回的目录数据不符合约定，已保留上一次列表。',
    'error.request': '请求失败，请重试。',
    'error.network': '网络中断，请检查连接后重试。',
    'error.generic': '操作失败，请重试。'
  };

  var EN = {
    sessionRemaining: '{time} left',
    sessionChecking: 'Checking session…',
    connectTitle: 'Connect to shared folder',
    connectBodyPairing: 'Establishing a session with Kerminal…',
    connectBodyMissing: 'Return to the LAN File Transfer panel in Kerminal, copy the share link, and open it again.',
    connectBodyRejected: 'This share link is invalid or has expired. Copy a new link in Kerminal.',
    connectBodyUnreachable: 'Cannot reach the service. Make sure Kerminal is still running, then retry.',
    connectBodyRateLimited: 'The service is rate limiting requests. Wait a moment and try again.',
    connectBodyUnconfirmed: 'Cannot confirm the status of this share link right now. Try again shortly.',
    connectBodyExpired: 'The session is no longer valid. Copy a new share link in Kerminal.',
    connectBodyDisconnected: 'Disconnected. Copy a new share link in Kerminal to continue.',
    connectBodyDeferred: 'Not connected to a shared folder. The new share link received just now is still held on this page; use it to pair.',
    linkNoticeActiveSession: 'A new share link was detected, but the current session is still active. Disconnect first, then use the new link.',
    linkNoticePairing: 'A connection is in progress, so the new share link was not used. Open the new link after this attempt finishes.',
    linkNoticeUse: 'Use this link',
    linkNoticeInvalid: 'The new fragment is not a valid share link and was cleared.',
    linkNoticeDismiss: 'Dismiss',
    retry: 'Retry',
    reconnect: 'Reconnect',
    disconnect: 'Disconnect',
    disconnectHint: 'Ends this page\'s connection to the shared folder. No files are deleted.',
    breadcrumbLabel: 'Current location',
    goUp: 'Up',
    goUpHint: 'Go to parent folder (Alt+↑)',
    goUpTo: 'Go up to {name} (Alt+↑)',
    refresh: 'Refresh',
    refreshHint: 'Refresh current folder',
    shareTitle: 'Shared folder',
    colName: 'Name',
    colSize: 'Size',
    colModified: 'Modified',
    colAction: 'Action',
    loadMore: 'Load more',
    loading: 'Loading…',
    itemsCount: '{count} items',
    emptyDir: 'This folder is empty. Choose files to upload.',
    open: 'Open',
    download: 'Download',
    downloading: 'Preparing…',
    downloadStarted: 'Handed to the browser; whether it is saved is up to the browser.',
    downloadFailed: 'Download did not start',
    selectFiles: 'Choose files',
    dropHint: 'Drop files anywhere to upload',
    dropHere: 'Drop to upload into the current folder',
    uploadsTitle: 'Uploads',
    uploadTarget: 'Uploads · {path}',
    uploadMultipleTargets: 'Uploads · multiple folders',
    uploadQueued: 'Queued',
    uploadInProgress: 'Uploading…',
    uploadProgress: '{percent}%',
    uploadAwaiting: 'Transfer complete, waiting for server confirmation…',
    uploadSubmitted: 'Submitted',
    uploadSubmittedAs: 'Submitted as {name}',
    uploadCancelled: 'Cancelled',
    uploadUncertain: 'Outcome unknown; check the target folder',
    uploadUncertainHint: 'Cannot confirm whether this file was uploaded. Use "Check folder" to open its original target folder; nothing is re-uploaded automatically.',
    viewTargetDir: 'Check folder',
    uploadSummary: '{count} files · {size} · {done} submitted',
    uploadSummaryUncertain: '{count} unconfirmed',
    cancel: 'Cancel',
    cancelAll: 'Cancel all',
    clearDone: 'Clear finished',
    clearDoneHint: 'Clears finished items; unconfirmed records stay and can be removed individually',
    remove: 'Remove',
    removeRecord: 'Remove record',
    removeRecordHint: 'Removes this unconfirmed record from this page only; no file on the server is deleted.',
    themeAuto: 'Auto',
    themeLight: 'Light',
    themeDark: 'Dark',
    themeTitle: 'Switch theme',
    langTitle: 'Switch language',
    serviceUnreachable: 'Cannot reach the service. Make sure Kerminal is still running.',
    rootName: 'Shared root',
    'error.unauthorized': 'Session is no longer valid. Reopen the share link.',
    'error.session_expired': 'Session expired. Reopen the share link.',
    'error.invalid_secret': 'Pairing failed. The share link may have expired.',
    'error.forbidden': 'This action is not allowed.',
    'error.not_found': 'File or folder not found.',
    'error.dir_not_found': 'The folder does not exist or is outside the shared root.',
    'error.ticket_expired': 'The download ticket expired. Try again.',
    'error.too_large': 'The file exceeds the size limit.',
    'error.busy': 'The service is busy. Try again shortly.',
    'error.conflict': 'A file with this name exists and could not be renamed automatically.',
    'error.quota': 'Not enough storage space.',
    'error.server': 'Internal service error. Try again later.',
    'error.internal': 'Internal service error. Try again later.',
    'error.unavailable': 'The service is temporarily unavailable.',
    'error.bad_request': 'Invalid request.',
    'error.path_rejected': 'That path is not allowed.',
    'error.protocol': 'The folder data from the service does not match the contract; the previous listing was kept.',
    'error.request': 'Request failed. Try again.',
    'error.network': 'Network interrupted. Check the connection and retry.',
    'error.generic': 'The operation failed. Try again.'
  };

  /**
   * 本地化查表：先查当前语言，再回退中文，最后返回 key 本身。
   * 回退而不是抛错，是为了新增文案漏翻译时页面仍然可用。
   */
  function translate(locale, key, vars) {
    var dict = locale === 'en' ? EN : ZH;
    var text = dict[key];
    if (typeof text !== 'string') text = ZH[key];
    if (typeof text !== 'string') return key;
    if (vars) {
      var names = Object.keys(vars);
      for (var i = 0; i < names.length; i += 1) {
        text = text.split('{' + names[i] + '}').join(String(vars[names[i]]));
      }
    }
    return text;
  }

  /* ------------------------------------------------------------------ */
  /* DOM 逻辑                                                            */
  /* ------------------------------------------------------------------ */

  /**
   * 启动页面。只在真实浏览器中调用；所有凭据只存在于闭包内的 state，
   * 既不落存储也不进日志。片段在任何请求发出前就被清除，避免被历史记录或转发带出。
   */
  function bootPage(win, doc) {
    var state = {
      token: null,
      expiresAt: null,
      pendingSecret: null,
      connectMode: 'missing',
      locale: 'zh',
      theme: 'auto',
      dir: 'root',
      cursor: null,
      nextCursor: null,
      breadcrumbs: [],
      entries: [],
      loading: false,
      listError: null,
      listSeq: 0,
      // 失败请求的原样参数：重试必须按原请求类型（首页刷新/翻页/刷新保留）重发，
      // 否则刷新失败后会用 reset=false 且 nextCursor 已清空，重试按钮变成空操作。
      pendingListRequest: null,
      listController: null,
      // 上传提交后需要刷新的目录集合。列表正在加载时不丢弃，等加载结束再合并成一次刷新。
      pendingRefreshDirs: Object.create(null),
      uploads: [],
      activeUploads: 0,
      uploadSeq: 0,
      // 上传所属会话代次。会话结束后旧请求的迟到回调不能释放新会话的并发槽，
      // 也不能用旧目录触发新会话的刷新。
      uploadEpoch: 0,
      uploadNodes: new Map(),
      // 全部取消期间为 true：abort 同步触发的完成回调不能把仍排队的项重新发出去。
      cancelAllActive: false,
      // 会话结束时为 true：此时的中断是主动清理，不应显示“结果待核对”或触发刷新。
      endingSession: false,
      // 会话校验同一时刻只允许一个在途请求；否则每秒 tick、恢复可见和手动重试会叠加成并发请求。
      sessionCheckInFlight: false,
      // 用户在检查在途时点过重试：只排队一次，当前检查落地后补发，不并发也不丢点击。
      sessionCheckQueued: false,
      // 自动校验的冷却截止时间。服务端仍返回已过去的时间时，用它挡住每秒 tick 的重复触发，
      // 周期定时器仍按固定间隔重试，不会把一次“无法确认”变成请求风暴。
      expiryCheckAfter: 0,
      lastSessionCheckAt: 0,
      // 校验请求的序号用于归属判断：旧会话迟到的响应不能清除新会话的在途标记。
      sessionCheckSeq: 0,
      // 配对请求同样只允许一个在途，且用序号丢弃旧响应，避免连点重试把旧结果当成新会话。
      pairingInFlight: false,
      pairSeq: 0,
      // 同页 hashchange 收到的新密钥。只在内存里停留：活跃会话期间不能自动切换，
      // 待用户显式断开后，连接页再用它发起配对。
      newLinkSecret: null,
      // 当前可见的新链接提示类型（active/pairing/invalid），空表示不显示。
      linkNoticeKind: null,
      tickTimer: null,
      checkTimer: null,
      checkFailures: 0,
      flashTimer: null
    };

    /**
     * 页面元素在启动时一次性取好，后续渲染不再查 DOM；元素缺失说明 HTML 与脚本不匹配，直接取到 null 会在使用处暴露。
     */
    function byId(id) {
      return doc.getElementById(id);
    }

    var els = {
      sessionChip: byId('session-chip'),
      sessionChipText: byId('session-chip-text'),
      langBtn: byId('lang-btn'),
      themeBtn: byId('theme-btn'),
      disconnectBtn: byId('disconnect-btn'),
      connectView: byId('connect-view'),
      connectSpinner: byId('connect-spinner'),
      connectTitle: byId('connect-title'),
      connectBody: byId('connect-body'),
      connectRetry: byId('connect-retry'),
      browseView: byId('browse-view'),
      browseBanner: byId('browse-banner'),
      browseBannerText: byId('browse-banner-text'),
      browseBannerAction: byId('browse-banner-action'),
      shareTitle: byId('share-title'),
      upBtn: byId('up-btn'),
      upBtnLabel: byId('up-btn-label'),
      refreshBtn: byId('refresh-btn'),
      breadcrumbs: byId('breadcrumbs'),
      uploadBtn: byId('upload-btn'),
      fileInput: byId('file-input'),
      uploadHint: byId('upload-hint'),
      uploadSummary: byId('upload-summary'),
      uploads: byId('uploads'),
      uploadsTitle: byId('uploads-title'),
      cancelAll: byId('cancel-all'),
      clearDone: byId('clear-done'),
      uploadList: byId('upload-list'),
      listCard: byId('list-card'),
      listHead: byId('list-head'),
      listStatus: byId('list-status'),
      listCount: byId('list-count'),
      listRetry: byId('list-retry'),
      entries: byId('entries'),
      listFoot: byId('list-foot'),
      loadMore: byId('load-more'),
      dropOverlay: byId('drop-overlay'),
      linkNotice: byId('link-notice'),
      linkNoticeText: byId('link-notice-text'),
      linkNoticeDismiss: byId('link-notice-dismiss')
    };

    var dragDepth = 0;

    /**
     * 把当前语言固定进查表，调用点不必重复传 locale；语言切换只改 state 并整体重渲染。
     */
    function t(key, vars) {
      return translate(state.locale, key, vars);
    }

    /**
     * 服务端错误码只用于查本地化文案，查不到时回退到通用失败提示，绝不把原始 message 显示给用户。
     */
    function errorText(code) {
      var key = 'error.' + (code || 'generic');
      var text = t(key);
      return text === key ? t('error.generic') : text;
    }

    /**
     * 偏好读取失败（隐私模式或存储被禁用）时按默认值处理；偏好不承担安全职责，不能因此阻断页面。
     */
    function readStored(key) {
      try {
        return win.localStorage.getItem(key);
      } catch (error) {
        return null;
      }
    }

    /**
     * 写入失败静默忽略：主题/语言只是本地偏好，存储异常不应影响浏览和传输。
     */
    function writeStored(key, value) {
      try {
        win.localStorage.setItem(key, value);
      } catch (error) {
        /* 隐私模式下写入失败不影响使用，主题/语言只作为偏好，不承担安全职责。 */
      }
    }

    /**
     * 统一用 hidden 属性控制显隐，配合 CSS 的 [hidden] 兜底；不要用 style.display 直接写，否则会和媒体查询冲突。
     */
    function setHidden(node, hidden) {
      if (!node) return;
      if (hidden) node.setAttribute('hidden', '');
      else node.removeAttribute('hidden');
    }

    /**
     * 逐个子节点移除而不是 innerHTML=""，保证被移除节点上的事件监听随节点一起释放，也不会触发 HTML 解析。
     */
    function clearNode(node) {
      while (node && node.firstChild) node.removeChild(node.firstChild);
    }

    /* ---------------- 主题与语言 ---------------- */

    function applyTheme(theme) {
      state.theme = theme === 'light' || theme === 'dark' ? theme : 'auto';
      var rootEl = doc.documentElement;
      if (state.theme === 'auto') rootEl.removeAttribute('data-theme');
      else rootEl.setAttribute('data-theme', state.theme);
      var label = state.theme === 'light' ? t('themeLight')
        : state.theme === 'dark' ? t('themeDark') : t('themeAuto');
      els.themeBtn.textContent = label;
      els.themeBtn.setAttribute('title', t('themeTitle'));
      els.themeBtn.setAttribute('aria-label', t('themeTitle') + ': ' + label);
      writeStored('lan-transfer.theme', state.theme);
    }

    /**
     * 切语言要重算所有本地化文本，包括条目数、状态行和上传摘要；只替换静态标签会留下半中半英的界面。
     */
    function applyLocale(locale) {
      state.locale = locale === 'en' ? 'en' : 'zh';
      doc.documentElement.lang = state.locale === 'en' ? 'en' : 'zh-CN';
      els.langBtn.textContent = state.locale === 'en' ? '中文' : 'EN';
      els.langBtn.setAttribute('title', t('langTitle'));
      els.langBtn.setAttribute('aria-label', t('langTitle'));

      var localized = doc.querySelectorAll('[data-i18n]');
      for (var i = 0; i < localized.length; i += 1) {
        localized[i].textContent = t(localized[i].getAttribute('data-i18n'));
      }

      els.connectRetry.textContent = t('retry');
      els.disconnectBtn.textContent = t('disconnect');
      // “断开”紧邻语言/主题按钮，需说明断开的对象与无害性；用 title/aria 承载完整说明，
      // 按钮文字保持短，避免顶栏在窄屏被撑开。
      els.disconnectBtn.setAttribute('title', t('disconnectHint'));
      els.disconnectBtn.setAttribute('aria-label', t('disconnect') + ': ' + t('disconnectHint'));
      // 面包屑只有文件夹名时像输入框；用本地化的导航名称表明这是“当前位置”。
      els.breadcrumbs.setAttribute('aria-label', t('breadcrumbLabel'));
      if (els.upBtnLabel) els.upBtnLabel.textContent = t('goUp');
      if (els.upBtn) {
        els.upBtn.setAttribute('title', t('goUpHint'));
        els.upBtn.setAttribute('aria-label', t('goUp'));
      }
      if (els.refreshBtn) {
        els.refreshBtn.setAttribute('title', t('refreshHint'));
        els.refreshBtn.setAttribute('aria-label', t('refresh'));
      }
      els.uploadBtn.textContent = t('selectFiles');
      els.uploadHint.textContent = t('dropHint');
      els.uploadsTitle.textContent = t('uploadsTitle');
      els.cancelAll.textContent = t('cancelAll');
      els.clearDone.textContent = t('clearDone');
      els.listRetry.textContent = t('retry');
      els.browseBannerAction.textContent = t('reconnect');
      els.uploadSummary.setAttribute('aria-live', 'polite');
      // 空状态行和按钮的静态文案也要跟着换语言；否则切到英文后仍显示中文。
      els.shareTitle.textContent = t('shareTitle');
      els.loadMore.textContent = t('loadMore');
      // 主题按钮的“自动/浅色/深色”是本地化文案，切语言后必须重算，否则会停留在旧语言。
      applyTheme(state.theme);
      // 提示可能在浏览页（活跃会话）或连接页显示，必须在两条分支之前统一重渲染。
      renderLinkNotice();

      if (state.token) {
        updateSessionChip();
        renderBreadcrumbs();
        renderList();
        // 条目数和状态行也是本地化文本，切语言后必须一起重算，否则会残留旧语言。
        updateListHead();
        renderUploads();
        updateUploadSummary();
      } else {
        // 连接页整体重算（含 deferred 的“使用此链接”按钮文字），依赖当前语言。
        renderConnectCopy();
      }
      writeStored('lan-transfer.lang', state.locale);
    }

    /* ---------------- 会话与连接 ---------------- */

    function setConnectMode(mode) {
      state.connectMode = mode;
      renderConnectCopy();
    }

    /**
     * 连接页文案由状态驱动（配对中/链接无效/不可达/已过期/已断开），集中在一处保证 spinner、重试按钮与文案始终一致。
     */
    function renderConnectCopy() {
      var mode = state.connectMode;
      var title = t('connectTitle');
      var body = t('connectBodyMissing');
      var busy = mode === 'pairing';
      var canRetry = false;

      if (mode === 'pairing') body = t('connectBodyPairing');
      else if (mode === 'rejected') body = t('connectBodyRejected');
      else if (mode === 'unreachable') { body = t('connectBodyUnreachable'); canRetry = true; }
      // 未知状态码或无法解析的配对响应不能断言链接失效，必须保留密钥并给出可重试入口。
      else if (mode === 'unconfirmed') { body = t('connectBodyUnconfirmed'); canRetry = true; }
      // 限流是服务端在线前提下的暂时拒绝，单独成态：沿用 unreachable 的可重试交互，
      // 但文案明确“限流、可等待重试”，不误导用户去排查一个没有故障的服务。
      else if (mode === 'rate_limited') { body = t('connectBodyRateLimited'); canRetry = true; }
      else if (mode === 'expired') body = t('connectBodyExpired');
      else if (mode === 'disconnected') body = t('connectBodyDisconnected');
      // 断开时若刚收到过新链接，连接页直接提供“使用此链接”，由用户显式发起切换；
      // 不自动重连，避免把用户只是“断开”解读成“换用新会话”。
      else if (mode === 'deferred') { body = t('connectBodyDeferred'); canRetry = true; }

      els.connectTitle.textContent = title;
      els.connectBody.textContent = body;
      // 重试按钮的语义随状态变化：deferred 是“换成新链接”，其余仍是“重试当前链接”。
      els.connectRetry.textContent = mode === 'deferred' ? t('linkNoticeUse') : t('retry');
      setHidden(els.connectSpinner, !busy);
      setHidden(els.connectRetry, !canRetry);
      els.connectRetry.disabled = busy;
    }

    /**
     * 回到连接页时必须同时收起浏览区、会话标签和拖放遮罩，避免残留的旧会话界面被误认为仍然在线。
     */
    function showConnect(mode) {
      setConnectMode(mode);
      setHidden(els.browseView, true);
      setHidden(els.connectView, false);
      setHidden(els.sessionChip, true);
      setHidden(els.disconnectBtn, true);
      setHidden(els.dropOverlay, true);
    }

    /**
     * 会话剩余时间只用于显示和提前变色。本地时钟可能被手动改过或与时区/休眠不同步，
     * 因此倒计时归零不能直接判定服务端会话失效：那样会中止仍有效的上传和浏览。
     * 归零时只触发一次权威校验，由 /api/session 的 401 或失败来决定是否结束会话。
     */
    function updateSessionChip() {
      if (!state.token) {
        setHidden(els.sessionChip, true);
        return;
      }
      var remaining = formatRemaining(state.expiresAt, Date.now());
      setHidden(els.sessionChip, false);
      // 本地倒计时归零只说明“该向服务端确认了”，不证明会话已失效：会话是否失效只有
      // /api/session 能回答（确认失效会走 endSession 直接隐藏 chip），因此归零期间
      // 永远不进入红色 expired 态，沿用琥珀色和“正在确认”文案表达“不确定、正在核对”。
      els.sessionChip.classList.toggle('is-expiring', remaining.expiring || remaining.expired);
      els.sessionChipText.textContent = remaining.expired
        ? t('sessionChecking')
        : t('sessionRemaining', { time: remaining.text });
      if (remaining.expired && !state.sessionCheckInFlight && !state.sessionCheckQueued) {
        checkSession();
      }
    }

    /**
     * 倒计时用于展示，权威校验按固定间隔进行；两者分开是为了避免每秒请求服务端。
     * 进入会话时重置冷却与在途标记，保证新会话的第一轮检查不被上一会话的失败状态拖住。
     */
    function startTimers() {
      stopTimers();
      state.expiryCheckAfter = 0;
      state.lastSessionCheckAt = 0;
      state.sessionCheckQueued = false;
      state.tickTimer = win.setInterval(updateSessionChip, 1000);
      state.checkTimer = win.setInterval(function () { checkSession({ periodic: true }); }, SESSION_CHECK_INTERVAL_MS);
    }

    /**
     * 会话结束时清理定时器。页面隐藏时这里刻意不停表：浏览器会自行节流后台标签的定时器，
     * 恢复可见的校验已有最小间隔与在途去重挡住重复请求；若在这里按可见性反复停启，
     * 还要处理周期相位丢失与恢复时的补发逻辑，收益为零、复杂度为正。
     */
    function stopTimers() {
      if (state.tickTimer) { win.clearInterval(state.tickTimer); state.tickTimer = null; }
      if (state.checkTimer) { win.clearInterval(state.checkTimer); state.checkTimer = null; }
    }

    /**
     * 定期向服务端确认会话仍在有效期内。网络短暂失败不立刻判失效，
     * 连续失败达到阈值才显示“无法连接服务”，避免移动网络抖动时页面来回跳。
     *
     * 关键约束：
     * - 同一时刻只允许一个校验请求：tick、恢复可见与手动重试都会走到这里；
     * - 手动重试在途时只排队一次，等当前结果落地后补发，既不并发也不丢点击；
     * - 自动校验失败或服务端仍返回已过去的时间时，把下一次 tick 触发的校验推迟到
     *   周期间隔，避免每秒请求，也避免在成功回调里递归立刻再发；
     * - 周期定时器与手动重试不受该冷却限制，正常重试不会被误禁；
     * - 用发起时的令牌判定响应是否属于当前会话，旧会话的响应不得写入新会话状态。
     */
    function checkSession(options) {
      var opts = options || {};
      if (!state.token) return;
      var now = Date.now();
      if (!opts.force && !opts.periodic && !opts.resume && now < state.expiryCheckAfter) return;
      if (opts.resume && now - state.lastSessionCheckAt < VISIBILITY_CHECK_GAP_MS) return;
      if (state.sessionCheckInFlight) {
        if (opts.force) state.sessionCheckQueued = true;
        return;
      }

      var token = state.token;
      state.sessionCheckInFlight = true;
      state.lastSessionCheckAt = now;
      state.sessionCheckSeq += 1;
      var seq = state.sessionCheckSeq;

      // 旧会话的迟到回调不能清掉新会话的在途标记，否则会并发出两个校验请求。
      function release() {
        if (seq !== state.sessionCheckSeq) return false;
        state.sessionCheckInFlight = false;
        return true;
      }

      function rerunQueued() {
        if (!state.sessionCheckQueued) return;
        state.sessionCheckQueued = false;
        if (state.token === token) checkSession({ force: true });
      }

      apiFetch('/api/session').then(function (result) {
        if (!release()) return;
        // 401 时 apiFetch 已经结束会话；令牌变了说明这是旧会话的响应，不能再写状态。
        if (!state.token || state.token !== token) {
          state.sessionCheckQueued = false;
          return;
        }
        if (!result.ok) {
          if (result.status === 401) {
            endSession('expired');
            return;
          }
          // 非 401 的失败只代表“暂时问不到”。0 与 5xx 计入不可达阈值；
          // 其他状态说明服务端仍在响应，既不算连接故障，也不当成成功去收起已有横幅。
          if (result.status === 0 || result.status >= 500) registerCheckFailure();
          // 拉长 tick 触发的自动校验间隔；周期定时器和手动重试仍会继续。
          state.expiryCheckAfter = Date.now() + SESSION_CHECK_INTERVAL_MS;
          rerunQueued();
          return;
        }
        var payload = result.payload;
        var confirmed = payload && typeof payload.expiresAt === 'number' && isFinite(payload.expiresAt);
        if (confirmed) state.expiresAt = payload.expiresAt;
        state.checkFailures = 0;
        hideBanner();
        // 服务端仍给出已过去的时间时，本地只能继续显示“正在确认”；
        // 把下一次自动校验推迟到周期间隔，避免 updateSessionChip 在成功回调里立刻递归发请求。
        if (!confirmed || formatRemaining(state.expiresAt, Date.now()).expired) {
          state.expiryCheckAfter = Date.now() + SESSION_CHECK_INTERVAL_MS;
        } else {
          state.expiryCheckAfter = 0;
        }
        updateSessionChip();
        rerunQueued();
      }).catch(function () {
        if (!release()) return;
        if (!state.token || state.token !== token) {
          state.sessionCheckQueued = false;
          return;
        }
        registerCheckFailure();
        state.expiryCheckAfter = Date.now() + SESSION_CHECK_INTERVAL_MS;
        rerunQueued();
      });
    }

    /**
     * 连续失败达到阈值才提示不可达，避免移动网络抖动时提示条反复闪烁。
     */
    function registerCheckFailure() {
      state.checkFailures += 1;
      if (state.checkFailures >= UNREACHABLE_AFTER_FAILURES) {
        showBanner(t('serviceUnreachable'), true);
      }
    }

    /**
     * 服务级故障用独立横幅提示并给出重连入口，不占用列表状态行，避免把目录错误和连接错误混在一起。
     */
    function showBanner(text, withAction) {
      els.browseBannerText.textContent = text;
      els.browseBanner.classList.remove('banner-warn');
      els.browseBanner.classList.add('banner-error');
      setHidden(els.browseBanner, false);
      setHidden(els.browseBannerAction, !withAction);
    }

    /**
     * 任何一次成功响应都收起故障横幅，让恢复可见。
     */
    function hideBanner() {
      setHidden(els.browseBanner, true);
    }

    /**
     * 新链接提示独立于故障横幅：连接页与浏览页都要能看到，且不能被一次成功的列表/校验
     * 顺手收起，否则用户会以为新链接已经生效。这里只根据状态查文案，绝不把片段本身写进 DOM。
     */
    function renderLinkNotice() {
      var kind = state.linkNoticeKind;
      if (!kind) {
        setHidden(els.linkNotice, true);
        return;
      }
      els.linkNoticeText.textContent = kind === 'active'
        ? t('linkNoticeActiveSession')
        : kind === 'pairing'
          ? t('linkNoticePairing')
          : t('linkNoticeInvalid');
      els.linkNoticeDismiss.textContent = t('linkNoticeDismiss');
      setHidden(els.linkNotice, false);
    }

    function showLinkNotice(kind) {
      state.linkNoticeKind = kind;
      renderLinkNotice();
    }

    function hideLinkNotice() {
      state.linkNoticeKind = null;
      renderLinkNotice();
    }

    /**
     * 统一清理会话。先停定时器再取消上传，避免取消过程中触发的状态更新继续刷新界面；
     * 已进入提交阶段的上传不会被“取消”伪造成成功，只保留服务端返回的真实结果。
     */
    function endSession(mode) {
      stopTimers();
      state.endingSession = true;
      state.listSeq += 1;
      abortListRequest();
      state.pendingListRequest = null;
      state.pendingRefreshDirs = Object.create(null);
      // 在途校验的响应落地后会因令牌已清空而自行丢弃；这里同步重置标记，
      // 让下一次配对进入的新会话可以立即检查，而不是被上一会话的“在途”状态挡住。
      state.sessionCheckInFlight = false;
      state.sessionCheckQueued = false;
      state.sessionCheckSeq += 1;
      state.expiryCheckAfter = 0;
      state.lastSessionCheckAt = 0;
      state.pairSeq += 1;
      state.pairingInFlight = false;
      // 代次先递增再取消：abort 可能同步触发回调，旧代次的上传只能被忽略，
      // 不能在新会话里释放槽位或刷新目录。
      state.uploadEpoch += 1;
      cancelAllUploads(true);
      state.activeUploads = 0;
      state.token = null;
      state.expiresAt = null;
      state.pendingSecret = null;
      state.dir = 'root';
      state.cursor = null;
      state.nextCursor = null;
      state.breadcrumbs = [];
      state.entries = [];
      state.loading = false;
      state.listError = null;
      state.uploads = [];
      state.uploadNodes.forEach(function (nodes) { nodes.li.remove(); });
      state.uploadNodes.clear();
      setHidden(els.uploads, true);
      // 顶部汇总与上传列表同属会话级状态：只清列表不清汇总，重新配对后会带着
      // 上一会话的“N 个文件 · 已提交 N”进入新会话，等于伪造了从未发生过的统计。
      setHidden(els.uploadSummary, true);
      els.uploadSummary.textContent = '';
      if (els.upBtn) els.upBtn.disabled = true;
      if (els.refreshBtn) els.refreshBtn.disabled = false;
      hideBanner();
      clearNode(els.entries);
      clearNode(els.breadcrumbs);
      updateListHead();
      hideLinkNotice();
      // 活跃会话期间暂存的新密钥不能在断开时静默丢掉：连接页用它作为显式选项，
      // 让“先断开、再换新链接”有完整闭环（密钥仍只在内存中）。这里刻意不清空
      // newLinkSecret——清空会让“使用此链接”变成点了没反应的死按钮。
      if (state.newLinkSecret) {
        showConnect('deferred');
      } else {
        showConnect(mode || 'disconnected');
      }
      state.endingSession = false;
    }

    /**
     * 进入会话后不自动轮询目录，先取 root，用户后续导航再按需请求；
     * 这样服务端负载和移动端流量都可控。
     */
    function enterSession(token, expiresAt) {
      state.token = token;
      state.expiresAt = expiresAt;
      state.pendingSecret = null;
      // 已经进入新会话：暂存的新链接提示与密钥都已完成使命，不能留到下一个会话误用。
      state.newLinkSecret = null;
      hideLinkNotice();
      state.checkFailures = 0;
      // 新会话的第一轮校验不能被上一会话的冷却或“在途”标记挡住；
      // 递增序号让上一会话迟到的响应无法清除这里的标记。
      state.sessionCheckSeq += 1;
      state.sessionCheckInFlight = false;
      state.sessionCheckQueued = false;
      state.expiryCheckAfter = 0;
      state.lastSessionCheckAt = 0;
      setHidden(els.connectView, true);
      setHidden(els.browseView, false);
      setHidden(els.sessionChip, false);
      setHidden(els.disconnectBtn, false);
      // 先重置冷却与计时器，再渲染会话标签：过期会话会在 updateSessionChip 里
      // 立即发起权威校验，若顺序反过来会把这次校验的时间戳清掉。
      startTimers();
      updateSessionChip();
      loadDir('root', { reset: true });
    }

    /**
     * 配对密钥只在内存里停留到换取令牌为止；失败时按“服务不可用”“无法确认”和“链接无效”分开处理：
     * 前两者可重试且必须保留密钥，只有服务端明确否定才丢弃。
     *
     * 请求期间禁用重试并只允许一个在途配对，用序号丢弃旧响应：
     * 连点重试或慢响应晚到都不能把上一轮结果写进当前页面，也不能重复进入会话。
     */
    /**
     * 处理同页 fragment 变化。浏览器语义（Chromium 实测）：同源同路径仅换 fragment
     * 不重载页面、不重新执行脚本，只触发 hashchange——所以“重新打开新链接”在
     * 断开状态下会没有任何反应，密钥还会留在地址栏。这里补上这条入口。
     *
     * 安全与状态约束：
     * - 片段立即从地址栏清除（history.replaceState），失败再退回清空 hash：
     *   密钥只允许在内存停留，不能留在可被截图、转发或同步的历史里；
     * - 只解析、校验长度与字符，不预判服务端有效性；无效片段同样清除并提示，
     *   不能静默当作没看见；
     * - 有活跃会话时绝不自动切换：切换意味着丢掉在途上传与待核对记录，或者
     *   把旧请求的响应用新令牌写进新代次。新密钥先存内存，提示用户显式断开；
     * - 配对在途时不打断旧配对（pairSeq 会丢弃它的响应，静默丢失等于伪成功），
     *   同样只暂存并提示等待；
     * - 断开或空闲（连接页、无在途配对）时也不自动配对：配对会把旧连接页的
     *   错误文案换成新会话，这是一次状态切换，应由用户确认（点“使用此链接”）。
     */
    function handleHashChange() {
      var raw = win.location.hash || '';
      if (!raw) return;
      var parsed = parsePairingHash(raw);
      try {
        win.history.replaceState(null, '', win.location.pathname + win.location.search);
      } catch (error) {
        win.location.hash = '';
      }
      if (!parsed.ok) {
        state.newLinkSecret = null;
        showLinkNotice('invalid');
        return;
      }
      state.newLinkSecret = parsed.secret;
      if (state.token) {
        // 活跃会话：保留一切在途状态，等用户自己决定何时断开。
        showLinkNotice('active');
        return;
      }
      if (state.pairingInFlight) {
        // 在途配对：不打断，也不提前消费新密钥，避免旧响应被丢弃后新会话凭空出现。
        showLinkNotice('pairing');
        return;
      }
      // 断开/空闲：把新密钥作为可一键使用的选项呈现，不静默忽略。
      // 之前的“失效片段”提示可能还挂着，先收起，避免同时出现两条互相矛盾的说明。
      hideLinkNotice();
      showConnect('deferred');
    }

    /**
     * 消费暂存的新密钥发起配对。只由“使用此链接”按钮触达：把 fragment 变化到
     * 新会话之间隔一次用户确认，避免粘贴一个旧链接就覆盖用户可见的连接状态。
     * 连接页与提示横幅都归零后走标准 startPairing：所有归属序号（pairSeq）不变，
     * 在途旧响应仍会被丢弃，不会因为入口不同而绕过任何守卫。
     */
    function pairWithNewLink() {
      var secret = state.newLinkSecret;
      if (!secret || state.token || state.pairingInFlight) return;
      state.newLinkSecret = null;
      hideLinkNotice();
      startPairing(secret);
    }

    function startPairing(secret) {
      if (state.pairingInFlight) return;
      state.pendingSecret = secret;
      showConnect('pairing');
      var body;
      try {
        body = JSON.stringify({ secret: secret });
      } catch (error) {
        state.pendingSecret = null;
        showConnect('rejected');
        return;
      }
      state.pairingInFlight = true;
      state.pairSeq += 1;
      var seq = state.pairSeq;
      apiFetch('/api/pair', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: body
      }).then(function (result) {
        if (seq !== state.pairSeq) return;
        state.pairingInFlight = false;
        if (result.ok) {
          var session = validatePairResponse(result.payload);
          if (!session) {
            // 2xx 但响应体不可用：无法证明链接有效或失效，保留密钥供手动重试。
            showConnect('unconfirmed');
            return;
          }
          enterSession(session.token, session.expiresAt);
          return;
        }
        if (result.status === 0 || result.status === 408 || result.status >= 500) {
          // 网络失败、服务端超时与 5xx 只说明暂时问不到，不是链接无效；
          // 密钥保留在内存里，用户可手动重试。
          showConnect('unreachable');
          return;
        }
        if (result.status === 429) {
          // 限流的语义是“服务在响应、但暂时拒绝”：429 能到达就证明 Sidecar 仍然在线，
          // 不能说“无法连接服务”，也不能混用泛化的“无法确认”。单独成态，明确限流与可等待重试。
          showConnect('rate_limited');
          return;
        }
        // 只有服务端明确表示密钥无效才判定链接失效。403（来源被拒）或 400（请求体不合法）
        // 说明的是部署/契约问题，不是链接本身，必须保留密钥并允许重试，不能谎称“已过期”。
        var code = describeApiError(result.status, result.payload).code;
        if (result.status === 401 || code === 'invalid_secret' || code === 'session_expired') {
          state.pendingSecret = null;
          showConnect('rejected');
          return;
        }
        showConnect('unconfirmed');
      }).catch(function () {
        if (seq !== state.pairSeq) return;
        state.pairingInFlight = false;
        showConnect('unreachable');
      });
    }

    /**
     * 统一的同源请求封装。credentials 用 omit、redirect 用 error，
     * 是为了确保令牌只随 Authorization 头发给当前 Sidecar，
     * 不会因 Cookie 或重定向被带到别的主机；响应解析失败时返回 null 而不抛错。
     * 只有明确的 401 才算会话失效；其余失败交给调用方按自身语义提示。
     *
     * 401 还要核对发起请求时的令牌是否仍是当前令牌：旧会话的慢响应晚到时，
     * 不能把已经重新配对的新会话一起结束。
     */
    function apiFetch(path, options) {
      var opts = options || {};
      var requestToken = state.token;
      var headers = {};
      if (opts.headers) {
        var names = Object.keys(opts.headers);
        for (var i = 0; i < names.length; i += 1) headers[names[i]] = opts.headers[names[i]];
      }
      if (requestToken) headers.Authorization = 'Bearer ' + requestToken;
      var init = {
        method: opts.method || 'GET',
        headers: headers,
        body: opts.body,
        credentials: 'omit',
        cache: 'no-store',
        referrerPolicy: 'no-referrer',
        redirect: 'error'
      };
      if (opts.signal) init.signal = opts.signal;
      return win.fetch(path, init).then(function (response) {
        return response.text().then(function (text) {
          var payload = null;
          if (text) {
            try {
              payload = JSON.parse(text);
            } catch (error) {
              payload = null;
            }
          }
          if (response.status === 401 && path !== '/api/pair' && requestToken &&
              state.token === requestToken) {
            endSession('expired');
          }
          return { ok: response.ok, status: response.status, payload: payload };
        });
      });
    }

    /* ---------------- 目录与列表 ---------------- */

    function updatePathLabel() {
      var names = [];
      for (var i = 0; i < state.breadcrumbs.length; i += 1) {
        var name = state.breadcrumbs[i].name;
        if (name) names.push(name);
      }
      return names.length ? names.join(' / ') : t('shareTitle');
    }

    /**
     * 加载目录或下一页。用自增序号丢弃过期响应：
     * 用户快速切换目录时，先发出的慢请求不能覆盖后发请求的结果，
     * 否则面包屑和文件列表会指向不同目录。
     *
     * 关键约束：
     * - 加载中再导航时以最新用户意图为准：取消在途请求、丢弃旧响应，而不是把点击丢掉；
     * - 刷新失败不改变已确认的目录和面包屑，只有成功响应才替换，用户始终能回到根；
     * - 失败请求的参数原样保留在 pendingListRequest 里，重试按原类型（首页/翻页/刷新）重发。
     */
    function loadDir(dirId, options) {
      var opts = options || {};
      var reset = opts.reset !== false;
      if (!state.token) return;

      var request;
      if (reset) {
        request = {
          kind: 'reset',
          dir: dirId == null || dirId === '' ? 'root' : String(dirId),
          cursor: null
        };
      } else {
        // 翻页以已确认的 nextCursor 为准；没有下一页时点击不应发起空请求。
        if (!state.nextCursor) return;
        request = { kind: 'page', dir: state.dir, cursor: state.nextCursor };
      }
      requestList(request);
    }

    /**
     * 真正发起列表请求。旧请求被 abort，响应序号也会被替换，
     * 因此被取消的请求无论以什么方式落地都不会再写状态。
     *
     * 请求期间不改动已确认的目录、面包屑和条目：只有成功且通过契约校验的响应才整体替换。
     * 这样导航失败或 200 但响应体损坏时，用户仍看到上一次确认的列表，而不是被清空的“空目录”。
     */
    function requestList(request) {
      if (!state.token) return;
      abortListRequest();

      state.pendingListRequest = request;
      state.listSeq += 1;
      var seq = state.listSeq;
      var controller = typeof win.AbortController === 'function' ? new win.AbortController() : null;
      state.listController = controller;
      state.loading = true;
      state.listError = null;
      // 重新渲染一次：列表可能刚被清空，需要立刻显示“加载中”占位；
      // 已有条目时 renderList 仍按旧数据渲染，因此两种情况下都不会闪出“目录为空”。
      renderList();
      updateListHead();
      els.listCard.setAttribute('aria-busy', 'true');

      apiFetch(buildListUrl(request.dir, request.cursor), controller ? { signal: controller.signal } : null)
        .then(function (result) {
          if (seq !== state.listSeq || !state.token) return;
          state.listController = null;
          if (!result.ok) {
            state.listError = describeApiError(result.status, result.payload);
            if (state.listError.code === 'unauthorized' || state.listError.code === 'session_expired') {
              endSession('expired');
              return;
            }
            if (result.status === 0) registerCheckFailure();
            else state.checkFailures = 0;
          } else {
            var page = parseListPayload(result.payload);
            if (!page.ok) {
              // 200 但响应体不符合契约：这是协议错误，不是“空目录”。
              // 保留上一次已确认的列表、面包屑和游标，重试按钮按原请求重发。
              state.listError = { code: 'protocol', status: result.status };
            } else if (!listTargetMatches(page.breadcrumbs, request.dir)) {
              // 响应目录与请求目录不一致（包括翻页）：同样按协议错误处理，
              // 绝不把别的目录内容合并进当前列表，重试仍指向原请求。
              state.listError = { code: 'protocol', status: result.status };
            } else {
              if (request.kind === 'reset') {
                state.dir = request.dir;
                state.cursor = null;
                state.breadcrumbs = page.breadcrumbs;
                state.entries = page.entries;
              } else {
                state.cursor = request.cursor;
                state.entries = mergeEntries(state.entries, page.entries);
              }
              state.nextCursor = page.nextCursor;
              state.pendingListRequest = null;
              state.checkFailures = 0;
              hideBanner();
            }
          }
          state.loading = false;
          els.listCard.removeAttribute('aria-busy');
          renderBreadcrumbs();
          renderList();
          updateListHead();
          flushPendingRefreshes();
        })
        .catch(function (error) {
          if (seq !== state.listSeq || !state.token) return;
          // 被 abort 的请求已由新请求接管，不能把取消写成网络错误。
          if (error && error.name === 'AbortError') return;
          state.listController = null;
          state.loading = false;
          state.listError = { code: 'network', status: 0 };
          els.listCard.removeAttribute('aria-busy');
          registerCheckFailure();
          renderBreadcrumbs();
          renderList();
          updateListHead();
          flushPendingRefreshes();
        });
    }

    /**
     * 取消在途列表请求。fetch 没有内建取消时退化为“序号作废”，
     * 两条路径都保证旧响应不再写状态，因此这里不把取消能力当成正确性前提。
     */
    function abortListRequest() {
      var controller = state.listController;
      state.listController = null;
      if (controller) {
        try {
          controller.abort();
        } catch (error) {
          /* 已完成的请求再 abort 是空操作，不需要额外处理。 */
        }
      }
    }

    /**
     * 重试按失败请求的类型重发。首页/刷新失败重取当前目录并保留已确认内容，
     * 翻页失败重发同一游标；没有失败记录时才退回刷新当前目录。
     */
    function retryListRequest() {
      var request = state.pendingListRequest;
      if (!request) {
        loadDir(state.dir, { reset: true });
        return;
      }
      if (request.kind === 'page' && !state.nextCursor) {
        loadDir(state.dir, { reset: true });
        return;
      }
      requestList({
        kind: request.kind,
        dir: request.dir,
        cursor: request.kind === 'page' ? state.nextCursor : null
      });
    }

    /**
     * 列表头只承担两类信息：左侧是稳定的条目数，右侧是瞬时的加载/错误/操作反馈。
     * 两者分开是为了让下载反馈出现和消失时不改动条目数，也不会在空目录上方留下空白带。
     */
    function updateListHead() {
      var count = state.entries.length;
      els.listCount.textContent = count > 0 ? t('itemsCount', { count: count }) : '';
      els.listStatus.classList.remove('is-error');
      setHidden(els.listRetry, true);

      if (state.loading) {
        els.listStatus.textContent = t('loading');
      } else if (state.listError) {
        els.listStatus.textContent = errorText(state.listError.code);
        els.listStatus.classList.add('is-error');
        setHidden(els.listRetry, false);
      } else {
        els.listStatus.textContent = '';
      }
      els.listStatus.setAttribute('title', els.listStatus.textContent);
      setHidden(els.listFoot, !state.nextCursor);
      els.loadMore.disabled = state.loading;
      if (els.refreshBtn) els.refreshBtn.disabled = state.loading;

      setHidden(els.listHead, els.listCount.textContent === '' && els.listStatus.textContent === '');
    }

    /**
     * 空状态只在请求彻底结束且成功时显示“目录为空”。
     * 加载中或失败时不能再声称目录为空，否则会把“还没拿到结果”说成空目录，
     * 并在结果到达时让内容高度跳动。
     */
    function emptyStateText() {
      if (state.listError) return errorText(state.listError.code);
      if (state.loading) return t('loading');
      return t('emptyDir');
    }

    /**
     * 短暂操作反馈复用列表状态行，并在几秒后恢复为当前计数；
     * 不新增会挤压列表的提示条，避免内容上下跳动。
     */
    function flashStatus(text, isError) {
      // 会话结束或未连接时列表已隐藏，此时写状态行既不可见也会覆盖连接页的错误说明。
      if (!state.token) return;
      if (state.flashTimer) win.clearTimeout(state.flashTimer);
      els.listStatus.textContent = text;
      els.listStatus.setAttribute('title', text);
      els.listStatus.classList.toggle('is-error', !!isError);
      setHidden(els.listHead, false);
      state.flashTimer = win.setTimeout(function () {
        state.flashTimer = null;
        updateListHead();
      }, 4000);
    }

    /**
     * 图标用内联 SVG 而不是字体或外链图片，既满足 CSP 同源限制，也不依赖任何第三方资源。
     */
    function createIcon(kind) {
      var svg = doc.createElementNS('http://www.w3.org/2000/svg', 'svg');
      svg.setAttribute('viewBox', '0 0 24 24');
      svg.setAttribute('width', '15');
      svg.setAttribute('height', '15');
      svg.setAttribute('aria-hidden', 'true');
      svg.setAttribute('focusable', 'false');
      var path = doc.createElementNS('http://www.w3.org/2000/svg', 'path');
      if (kind === 'directory') {
        path.setAttribute('fill', 'currentColor');
        path.setAttribute('d', 'M3.5 6.2c0-1 .8-1.8 1.8-1.8h4.1l1.8 1.8h6.5c1 0 1.8.8 1.8 1.8v8.2c0 1-.8 1.8-1.8 1.8H5.3c-1 0-1.8-.8-1.8-1.8V6.2Z');
      } else {
        path.setAttribute('fill', 'none');
        path.setAttribute('stroke', 'currentColor');
        path.setAttribute('stroke-width', '1.6');
        path.setAttribute('stroke-linejoin', 'round');
        path.setAttribute('d', 'M6.5 3.2h6.3l4.7 4.7v12.9c0 .6-.5 1.1-1.1 1.1H6.5c-.6 0-1.1-.5-1.1-1.1V4.3c0-.6.5-1.1 1.1-1.1Zm6 .9v4.5h4.5');
      }
      svg.appendChild(path);
      return svg;
    }

    /**
     * 归一化面包屑名称：根节点的 `/` 或 `root` 属于协议占位符，展示时映射为本地化名称，
     * 避免在标题或路径栏里出现孤立斜杠。
     */
    function crumbDisplayName(crumb) {
      if (!crumb) return t('rootName');
      var name = typeof crumb.name === 'string' ? crumb.name.trim() : '';
      if (crumb.id === 'root') {
        return name && name !== 'root' && name !== '/' ? name : t('rootName');
      }
      return name || crumb.id;
    }

    /**
     * 仅基于已确认的面包屑返回上一级，避免按未落地的请求状态猜测父目录。
     */
    function goUp() {
      if (!state.token || state.breadcrumbs.length < 2) return;
      var parent = state.breadcrumbs[state.breadcrumbs.length - 2];
      if (parent && parent.id) loadDir(parent.id, { reset: true });
    }

    /**
     * 同步“上一级”按钮可用态与提示文案：处于根目录时保持禁用占位，
     * 进出子目录时导航栏几何结构不跳动。
     */
    function updateUpButton(crumbs) {
      if (!els.upBtn) return;
      if (crumbs.length >= 2) {
        var parentLabel = crumbDisplayName(crumbs[crumbs.length - 2]);
        var hint = t('goUpTo', { name: parentLabel });
        els.upBtn.disabled = false;
        els.upBtn.setAttribute('title', hint);
        els.upBtn.setAttribute('aria-label', hint);
      } else {
        els.upBtn.disabled = true;
        els.upBtn.setAttribute('title', t('goUpHint'));
        els.upBtn.setAttribute('aria-label', t('goUp'));
      }
    }

    /**
     * 面包屑每次整体重建，保证当前目录与列表始终来自同一份已确认状态；根名称优先用服务端返回的共享目录名。
     */
    function renderBreadcrumbs() {
      var container = els.breadcrumbs;
      clearNode(container);

      // 只有文件夹名的整条边框栏初看像输入框；在行首加一个可见的“当前位置”标签，
      // 让视觉用户也能一眼看出这是导航路径而不是可编辑字段。标签随每次渲染重建，
      // 语言切换会经 applyLocale 重渲染，因此中英文始终一致。
      var locationLabel = doc.createElement('span');
      locationLabel.className = 'crumbs-label';
      locationLabel.textContent = t('breadcrumbLabel');
      container.appendChild(locationLabel);

      var crumbs = state.breadcrumbs.slice();
      if (!crumbs.length) {
        crumbs.push({ id: 'root', name: t('rootName') });
      }

      var rawRoot = crumbs.length && typeof crumbs[0].name === 'string' ? crumbs[0].name.trim() : '';
      var rootName = rawRoot && rawRoot !== 'root' && rawRoot !== '/'
        ? rawRoot
        : t('shareTitle');
      els.shareTitle.textContent = rootName;
      updateUpButton(crumbs);

      for (var i = 0; i < crumbs.length; i += 1) {
        var crumb = crumbs[i];
        if (i > 0) {
          var separator = doc.createElement('span');
          separator.className = 'crumb-sep';
          separator.setAttribute('aria-hidden', 'true');
          separator.textContent = '›';
          container.appendChild(separator);
        }
        var wrap = doc.createElement('span');
        wrap.className = 'crumb';
        var button = doc.createElement('button');
        button.type = 'button';
        var label = crumbDisplayName(crumb);
        button.textContent = label;
        button.setAttribute('title', label);
        var isCurrent = i === crumbs.length - 1;
        // 当前目录也保留点击：导航请求挂起或失败时，面包屑是用户回到已确认目录的唯一入口，
        // 把当前项做成死按钮会让用户无法取消一次卡住的跳转。点击当前项等价于重新加载该目录。
        (function (targetId) {
          button.addEventListener('click', function () {
            loadDir(targetId, { reset: true });
          });
        })(crumb.id);
        if (isCurrent) {
          button.setAttribute('aria-current', 'page');
        }
        wrap.appendChild(button);
        container.appendChild(wrap);
      }
    }

    /**
     * 列表整体重建而不是增量补丁：条目数量有限，整体重建更不容易出现新旧数据混杂。
     */
    function renderList() {
      var body = els.entries;
      clearNode(body);

      if (!state.entries.length) {
        var emptyRow = doc.createElement('tr');
        var emptyCell = doc.createElement('td');
        emptyCell.colSpan = 4;
        emptyCell.className = 'list-empty';
        if (state.listError) emptyCell.classList.add('is-error');
        emptyCell.textContent = emptyStateText();
        emptyRow.appendChild(emptyCell);
        body.appendChild(emptyRow);
        return;
      }

      var fragment = doc.createDocumentFragment();
      for (var i = 0; i < state.entries.length; i += 1) {
        fragment.appendChild(createEntryRow(state.entries[i]));
      }
      body.appendChild(fragment);
    }

    /**
     * 行内容全部用 textContent 写入，文件名不参与 HTML 解析，避免恶意文件名注入脚本或破坏布局。
     */
    function createEntryRow(entry) {
      var row = doc.createElement('tr');
      var isDir = entry.kind === 'directory';
      if (isDir) row.className = 'is-dir-row';

      var nameCell = doc.createElement('td');
      nameCell.className = 'col-name';
      var nameWrap = doc.createElement('div');
      nameWrap.className = 'entry-name';

      var icon = doc.createElement('span');
      icon.className = 'entry-icon' + (isDir ? ' is-dir' : '');
      icon.appendChild(createIcon(entry.kind));
      nameWrap.appendChild(icon);

      if (isDir) {
        var nameButton = doc.createElement('button');
        nameButton.type = 'button';
        nameButton.className = 'entry-label is-dir';
        nameButton.textContent = entry.name;
        nameButton.setAttribute('title', entry.name);
        nameButton.setAttribute('aria-label', t('open') + ': ' + entry.name);
        (function (id) {
          nameButton.addEventListener('click', function () {
            loadDir(id, { reset: true });
          });
          // 整行可点进入子目录，减少鼠标与触屏瞄准成本；点击行内按钮时不重复触发。
          row.addEventListener('click', function (event) {
            var target = event && event.target;
            if (target) {
              if (target.tagName === 'BUTTON') return;
              if (typeof target.closest === 'function' && target.closest('button')) return;
            }
            loadDir(id, { reset: true });
          });
        })(entry.id);
        nameWrap.appendChild(nameButton);
      } else {
        var nameLabel = doc.createElement('span');
        nameLabel.className = 'entry-label is-file';
        nameLabel.textContent = entry.name;
        nameLabel.setAttribute('title', entry.name);
        nameWrap.appendChild(nameLabel);
      }
      nameCell.appendChild(nameWrap);
      row.appendChild(nameCell);

      var sizeCell = doc.createElement('td');
      sizeCell.className = 'col-size';
      sizeCell.textContent = isDir ? '—' : formatSize(entry.size);
      row.appendChild(sizeCell);

      var modifiedCell = doc.createElement('td');
      modifiedCell.className = 'col-modified';
      modifiedCell.textContent = formatModified(entry.modified);
      row.appendChild(modifiedCell);

      var actionCell = doc.createElement('td');
      actionCell.className = 'col-action';
      var action = doc.createElement('button');
      action.type = 'button';
      action.className = 'btn-link';
      if (isDir) {
        action.textContent = t('open');
        action.setAttribute('aria-label', t('open') + ': ' + entry.name);
        (function (id) {
          action.addEventListener('click', function () {
            loadDir(id, { reset: true });
          });
        })(entry.id);
      } else {
        action.textContent = t('download');
        action.setAttribute('aria-label', t('download') + ': ' + entry.name);
        (function (item) {
          action.addEventListener('click', function () {
            downloadEntry(item, action);
          });
        })(entry);
      }
      actionCell.appendChild(action);
      row.appendChild(actionCell);

      return row;
    }

    /**
     * 下载分两步：先换取短期票据，再把同源票据 URL 交给浏览器原生下载。
     * 不用 fetch 读响应体，是为了让大文件直接落盘、不占页面内存；
     * 状态文案只说“已交给浏览器”，不宣称文件已保存。
     */
    function downloadEntry(entry, button) {
      if (!state.token) return;
      var originalText = button.textContent;
      button.disabled = true;
      button.textContent = t('downloading');
      // 记录发起时的令牌：票据响应是异步的，断开或重新配对后迟到的成功响应
      // 不能把旧会话的下载交给新页面的浏览器上下文——那等于用已失效的授权
      // 触发一次用户没有再次确认的文件写入。
      var requestToken = state.token;
      apiFetch('/api/download-ticket', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ fileId: entry.id })
      }).then(function (result) {
        if (state.token !== requestToken) return;
        if (!result.ok) {
          flashStatus(t('downloadFailed') + ' · ' + errorText(describeApiError(result.status, result.payload).code), true);
          return;
        }
        var raw = result.payload && typeof result.payload.url === 'string' ? result.payload.url : null;
        var safe = isSafeTicketUrl(raw, win.location.origin);
        if (!safe) {
          flashStatus(t('downloadFailed'), true);
          return;
        }
        var link = doc.createElement('a');
        link.href = safe;
        link.rel = 'noopener';
        link.download = entry.name || '';
        doc.body.appendChild(link);
        link.click();
        link.remove();
        flashStatus(t('downloadStarted'), false);
      }).catch(function () {
        if (state.token !== requestToken) return;
        flashStatus(t('downloadFailed'), true);
      }).then(function () {
        // 按钮恢复不依赖令牌归属：row 属于当前 DOM，若会话已切换该行早已随列表重建消失，
        // 恢复一个已脱离文档的节点是无害空操作。
        button.disabled = false;
        button.textContent = originalText;
      });
    }

    /* ---------------- 上传 ---------------- */

    function addFiles(fileList) {
      if (!state.token) return;
      var files = [];
      for (var i = 0; i < fileList.length; i += 1) files.push(fileList[i]);
      if (!files.length) return;

      var targetDir = state.dir;
      var targetLabel = updatePathLabel();
      var epoch = state.uploadEpoch;
      for (var j = 0; j < files.length; j += 1) {
        state.uploadSeq += 1;
        state.uploads.push({
          id: state.uploadSeq,
          file: files[j],
          name: files[j].name,
          size: typeof files[j].size === 'number' ? files[j].size : 0,
          loaded: 0,
          state: 'queued',
          targetDir: targetDir,
          targetLabel: targetLabel,
          errorCode: null,
          submittedName: null,
          xhr: null,
          // abort() 抛错说明请求未必真的停下，此时不能释放并发槽；
          // 记录该标记只用于界面与清理判断，槽位要等真实终止回调才回收。
          abortUncertain: false,
          epoch: epoch,
          finished: false
        });
      }
      renderUploads();
      updateUploadSummary();
      pumpUploads();
    }

    /**
     * 派发下一个排队项。cancelAllActive 期间禁止启动：
     * abort 会同步触发完成回调，若此时把仍排队的项发出去，用户刚点的“全部取消”就失效了。
     */
    function pumpUploads() {
      if (!state.token || state.cancelAllActive) return;
      // 槽位可能被“abort 抛错但请求仍在途”的上传占着；这是刻意保守，
      // 宁可让后续文件多等一会，也不能让真实并发超过上限。
      while (state.activeUploads < MAX_CONCURRENT_UPLOADS) {
        var next = null;
        for (var i = 0; i < state.uploads.length; i += 1) {
          if (state.uploads[i].state === 'queued') { next = state.uploads[i]; break; }
        }
        if (!next) return;
        startUpload(next);
      }
    }

    /**
     * 启动单个上传。open / setRequestHeader / responseType / send 都可能同步抛错
     * （状态非法、URL 不可用、请求被浏览器策略拒绝等），任何一步失败都必须释放占位并继续队列，
     * 否则并发槽会被永久占用，后面的文件永远停在“排队中”。
     */
    function startUpload(item) {
      // 会话结束时队列可能还剩待发项；此时不发无鉴权请求，直接按取消处理。
      if (!state.token) {
        item.state = 'cancelled';
        renderUploads();
        updateUploadSummary();
        return;
      }
      if (state.cancelAllActive) return;

      item.state = 'uploading';
      item.errorCode = null;
      item.finished = false;
      state.activeUploads += 1;
      renderUploads();

      var xhr;
      try {
        xhr = new win.XMLHttpRequest();
        item.xhr = xhr;
        xhr.open('POST', buildUploadUrl(item.targetDir, item.name), true);
        if (state.token) xhr.setRequestHeader('Authorization', 'Bearer ' + state.token);
        xhr.setRequestHeader('Content-Type', 'application/octet-stream');
        xhr.responseType = 'text';
      } catch (error) {
        // 请求还没有发出去，可以确定服务端没有收到这个文件。
        item.xhr = null;
        item.state = 'error';
        item.errorCode = 'request';
        finishUpload(item);
        return;
      }

      xhr.upload.onprogress = function (event) {
        if (!isCurrentUpload(item)) return;
        if (event.lengthComputable) {
          item.loaded = event.loaded;
          if (event.total > 0) item.size = event.total;
          updateUploadNode(item);
        }
      };

      xhr.onload = function () {
        // 会话已结束或已重新配对时，旧请求的回调不能再改新会话的界面与槽位。
        if (!isCurrentUpload(item)) return;
        var payload = null;
        if (xhr.responseText) {
          try {
            payload = JSON.parse(xhr.responseText);
          } catch (error) {
            payload = null;
          }
        }
        if (xhr.status >= 200 && xhr.status < 300) {
          var confirmed = validateUploadResponse(payload);
          if (confirmed) {
            item.state = 'done';
            item.loaded = item.size;
            item.submittedName = confirmed.name;
            item.size = confirmed.size;
            // 提交成功后刷新当前目录，让新文件出现在列表里；列表正在加载时由
            // refreshAfterUpload 合并到本次加载结束之后，不会被丢弃。
            refreshAfterUpload(item.targetDir);
          } else {
            // 2xx 但没有可用的确认体：服务端可能已经落盘，不能显示“已提交”，
            // 也不能断言失败，只能标成待核对并刷新目录去观察真实结果。
            item.state = 'uncertain';
            item.errorCode = null;
            refreshAfterUpload(item.targetDir);
          }
        } else if (xhr.status === 401) {
          item.state = 'error';
          item.errorCode = 'unauthorized';
          endSession('expired');
        } else if (uploadStatusIsUncertain(xhr.status)) {
          // 请求已完整发出后遇到 0、408 或 5xx：服务端可能已经写入再报错，
          // 也可能根本没收到。这里不能给确定失败，只能待核对并刷新。
          markUploadUncertain(item);
        } else {
          item.state = 'error';
          item.errorCode = describeApiError(xhr.status, payload).code;
        }
        finishUpload(item);
      };

      /**
       * 网络中断发生在请求已发出之后，服务端可能已经收到并提交了完整文件，
       * 因此不能显示“失败（必未提交）”；标为待核对并刷新目录，让用户看到真实结果。
       */
      xhr.onerror = function () {
        if (!isCurrentUpload(item)) return;
        markUploadUncertain(item);
        finishUpload(item);
      };

      /**
       * 主动 abort 分两种情况：
       * - 会话结束时的清理是确定取消，不必提示待核对；
       * - 用户在途取消时请求可能已完整送达服务端，同样只能给出待核对状态并刷新。
       */
      xhr.onabort = function () {
        if (!isCurrentUpload(item)) return;
        if (state.endingSession) {
          item.state = 'cancelled';
        } else {
          markUploadUncertain(item);
        }
        finishUpload(item);
      };

      try {
        xhr.send(item.file);
      } catch (error) {
        // send 同步抛错说明请求没有真正派发，服务端不会收到。
        item.state = 'error';
        item.errorCode = 'request';
        finishUpload(item);
      }
    }

    /**
     * 判断上传回调是否仍属于当前会话。会话结束会先递增代次再取消上传，
     * 因此旧请求的迟到回调既不能释放新会话的并发槽，也不能刷新新会话的目录。
     */
    function isCurrentUpload(item) {
      return item.epoch === state.uploadEpoch;
    }

    /**
     * 判断一个已发出请求的响应能否断言“服务端一定没有提交”。
     * 只有明确的 2xx（另走确认体校验）和提交前的拒绝码才算确定结果；
     * 0（拿不到响应）、408（服务端中途放弃）、5xx（可能已落盘后报错）、
     * 3xx 以及未知状态都只能标为待核对，避免把未知说成失败或成功。
     */
    function uploadStatusIsUncertain(status) {
      if (status >= 200 && status < 300) return false;
      var definiteRejection = status === 400 || status === 401 || status === 403 ||
        status === 404 || status === 409 || status === 413 || status === 429 || status === 507;
      return !definiteRejection;
    }

    /**
     * 进入“结果待核对”状态。这里不写 errorCode，因为界面文案要说明的是
     * “不知道是否已提交”，而不是某个确定的失败原因；同时请求刷新目录以观察真实结果。
     */
    function markUploadUncertain(item) {
      item.state = 'uncertain';
      item.errorCode = null;
      refreshAfterUpload(item.targetDir);
    }

    /**
     * 释放并发槽并刷新界面。用 finished 标记保证每个上传只结算一次：
     * onload/onerror/onabort 可能被同一个请求先后触发，重复结算会把 activeUploads 减成负数。
     * 只有真实终止事件或会话结束才走到这里；abort() 抛错本身不算终止。
     */
    function finishUpload(item) {
      if (item.finished) return;
      // 会话结束会先把所有上传置为已结算并清空队列；旧代次的迟到回调
      // 不能再释放新会话的槽位，也不能渲染已不存在的行。
      if (!isCurrentUpload(item)) return;
      item.finished = true;
      item.xhr = null;
      item.abortUncertain = false;
      releaseUploadSlot(item);
      renderUploads();
      updateUploadSummary();
      pumpUploads();
    }

    /**
     * 只回收并发占位，不做渲染和派发；调用前必须确认该请求确实已终止。
     */
    function releaseUploadSlot(item) {
      state.activeUploads = Math.max(0, state.activeUploads - 1);
    }

    /**
     * 上传完成后刷新当前目录。列表正在加载时不能定一个短延时后放弃：
     * 慢网络下列表可能几百毫秒都还没结束，文件就会在本页消失。
     * 这里把目录记入待刷新集合，由列表请求结束时合并成一次刷新，既不丢刷新也不堆积请求。
     */
    function refreshAfterUpload(targetDir) {
      // 会话结束清理期间不再发起任何列表请求：此时令牌可能仍短暂存在，
      // 但页面已经准备回到连接页，补发请求只会制造无归属的响应。
      if (!state.token || state.endingSession) return;
      state.pendingRefreshDirs[targetDir] = true;
      if (state.loading) return;
      flushPendingRefreshes();
    }

    /**
     * 列表请求结束后消费待刷新目录。只有当前目录才值得刷新，
     * 其余目录的条目会在用户再次进入时重新拉取；同一时刻最多合并成一次请求。
     */
    function flushPendingRefreshes() {
      if (!state.token || state.loading) return;
      var dirs = state.pendingRefreshDirs;
      state.pendingRefreshDirs = Object.create(null);
      if (!dirs[state.dir]) return;
      requestList({ kind: 'reset', dir: state.dir, cursor: null });
    }

    /**
     * 取消只表达“本端停止传输”；已经进入服务端提交阶段的上传不能被伪造成已取消，交由 onabort 统一判定为结果待核对。
     *
     * abort() 同步抛错不等于请求真的停了：浏览器可能仍在发送，服务端也可能已提交。
     * 因此这时只把界面改成“结果待核对”，不释放并发槽、不写“已取消”、也不宣称资源已释放。
     * 槽位要等真实的 onload/onerror/onabort 到达才回收；若浏览器始终不再回调，
     * 该槽位会一直占着直到会话结束（endSession 统一清理并作废旧代次回调），这是有意的保守取舍。
     */
    function cancelUpload(item) {
      if (item.state === 'queued') {
        item.state = 'cancelled';
        renderUploads();
        updateUploadSummary();
        return;
      }
      // abort 抛错后请求可能仍在途，允许用户再次尝试取消；已结算的记录不再响应。
      if (item.finished || !item.xhr) return;
      if (item.state !== 'uploading' && !item.abortUncertain) return;
      try {
        item.xhr.abort();
      } catch (error) {
        // 请求可能仍在途，保留槽位与 xhr 引用，等待真实终止事件。
        item.abortUncertain = true;
        // 重复点击取消时状态已是待核对，不必再触发一次目录刷新。
        if (item.state !== 'uncertain') markUploadUncertain(item);
        renderUploads();
        updateUploadSummary();
      }
    }

    /**
     * 全部取消。abort 会同步触发 onabort，而 onabort 又会调用 pumpUploads；
     * 若此时队列里还有未处理的 queued 项，就会被重新发出去。
     * 因此先整体标记取消、再逐个 abort，并用 cancelAllActive 挡住重入派发。
     *
     * abort() 抛错不代表请求停了：此时既不释放并发槽，也不把结果写成“已取消”。
     * 界面只标待核对，真实槽位等迟到的终止回调回收；会话结束时由 endSession
     * 递增代次统一清理，旧回调不会再影响新会话。
     */
    function cancelAllUploads(silent) {
      state.cancelAllActive = true;
      try {
        for (var i = 0; i < state.uploads.length; i += 1) {
          var item = state.uploads[i];
          if (item.state === 'queued') {
            item.state = 'cancelled';
          }
        }
        for (var j = 0; j < state.uploads.length; j += 1) {
          var current = state.uploads[j];
          // abortUncertain 表示上一次 abort 抛错、请求可能仍在途，这里仍应再试一次。
          if ((current.state === 'uploading' || current.abortUncertain) && current.xhr) {
            try {
              current.xhr.abort();
            } catch (error) {
              current.abortUncertain = true;
              // 会话结束的清理属于主动放弃，不必再提示待核对；
              // 用户手动全部取消时，请求可能已提交，只能给待核对。
              if (state.endingSession) current.state = 'cancelled';
              else if (current.state !== 'uncertain') markUploadUncertain(current);
            }
          }
        }
      } finally {
        state.cancelAllActive = false;
      }
      if (!silent) {
        renderUploads();
        updateUploadSummary();
      }
    }

    /**
     * 上传确认体契约是 {name, size}。只有形状合法时才把上传当作“已提交”：
     * 空响应、HTML 错误页或字段类型不对，都不能伪造成成功。
     */
    function validateUploadResponse(payload) {
      if (!payload || typeof payload !== 'object' || Array.isArray(payload)) return null;
      if (typeof payload.name !== 'string' || payload.name === '') return null;
      if (typeof payload.size !== 'number' || !isFinite(payload.size) || payload.size < 0) return null;
      return { name: payload.name, size: payload.size };
    }

    /**
     * 上传状态文案严格区分排队、传输中、传输完成待确认、已提交、已取消与结果待核对，不允许把传输完成说成已保存。
     */
    function uploadStateText(item) {
      if (item.state === 'queued') return t('uploadQueued');
      if (item.state === 'cancelled') return t('uploadCancelled');
      if (item.state === 'uncertain') return t('uploadUncertain');
      if (item.state === 'done') {
        return item.submittedName && item.submittedName !== item.name
          ? t('uploadSubmittedAs', { name: item.submittedName })
          : t('uploadSubmitted');
      }
      if (item.state === 'error') return errorText(item.errorCode);
      if (item.size > 0 && item.loaded >= item.size) return t('uploadAwaiting');
      if (item.size > 0) {
        var percent = Math.max(0, Math.min(100, Math.floor((item.loaded / item.size) * 100)));
        return t('uploadProgress', { percent: percent });
      }
      return t('uploadInProgress');
    }

    /**
     * 每个上传项只创建一次节点并复用，进度更新只改文本和宽度，避免高频进度事件反复重建 DOM 造成抖动。
     */
    function createUploadNode(item) {
      var li = doc.createElement('li');
      li.className = 'upload-item';
      li.setAttribute('data-upload-id', String(item.id));

      var row = doc.createElement('div');
      row.className = 'upload-row';

      var name = doc.createElement('span');
      name.className = 'upload-name';
      name.textContent = item.name;
      name.setAttribute('title', item.name);
      row.appendChild(name);

      var status = doc.createElement('span');
      status.className = 'upload-state';
      row.appendChild(status);

      var actions = doc.createElement('span');
      actions.className = 'upload-actions';
      var cancel = doc.createElement('button');
      cancel.type = 'button';
      cancel.className = 'btn-link';
      cancel.textContent = t('cancel');
      cancel.addEventListener('click', function () { cancelUpload(item); });
      actions.appendChild(cancel);

      // 待核对记录的核对闭环：用户需要一条能直达该上传原目标目录的操作，
      // 否则“请查看目标目录”的文案在当前目录不匹配时无法落地（面包屑呈当前页样式、
      // 无刷新提示）。按钮只导航/刷新目录，不重传、不推断结果、不自动移除记录。
      var checkFolder = doc.createElement('button');
      checkFolder.type = 'button';
      checkFolder.className = 'btn-link';
      checkFolder.textContent = t('viewTargetDir');
      checkFolder.addEventListener('click', function () { openUploadTargetDir(item); });
      actions.appendChild(checkFolder);

      // 待核对记录不能被“清除已完成”静默丢弃，因此单独提供“移除”入口；
      // 文案只表达“从本页移除这条记录”，不暗示成功或服务端已核对。
      var remove = doc.createElement('button');
      remove.type = 'button';
      remove.className = 'btn-link';
      remove.textContent = t('remove');
      remove.addEventListener('click', function () { removeUploadRecord(item); });
      actions.appendChild(remove);
      row.appendChild(actions);

      li.appendChild(row);

      var bar = doc.createElement('div');
      bar.className = 'upload-bar';
      // 进度条对辅助技术不是装饰：进行中要有可读的进度语义，结算后要让读屏能区分
      // “已提交”“待核对”“失败”，否则只剩视觉颜色，颜色本身不构成可访问状态。
      bar.setAttribute('role', 'progressbar');
      bar.setAttribute('aria-valuemin', '0');
      bar.setAttribute('aria-valuemax', '100');
      var fill = doc.createElement('div');
      fill.className = 'upload-bar-fill';
      bar.appendChild(fill);
      li.appendChild(bar);

      els.uploadList.appendChild(li);
      return { li: li, name: name, status: status, cancel: cancel, checkFolder: checkFolder, remove: remove, bar: bar, fill: fill };
    }

    /**
     * “查看目录”：导航（或刷新）到该待核对上传的原目标目录。
     * 设计约束：
     * - 用发起上传时记录的 targetDir，而不是当前所在目录：跨目录上传的核对必须落在
     *   原目标目录，否则用户看到的列表与这条记录无关；
     * - 已在该目录时也显式重发一次 reset 请求，让“查看”永远给出新鲜结果；
     * - 只改变列表位置，记录的状态、条目和移除入口保持原样，绝不自动重传或删除；
     * - 目标不可访问（404、网络错误等）走 loadDir 既有的可见错误路径（列表状态行 +
     *   重试按钮），记录继续保留供用户稍后再次核对；
     * - 归属三重校验（令牌、上传代次、记录仍在集合中）：重新配对后旧界面的节点仍挂在
     *   DOM 上且闭包引用还在，仅检查 token 非空会放行旧记录用新令牌导航；代次与集合
     *   检查保证只有当前会话、当前代次的在册记录才能触发导航；
     * - targetDir 缺失或失效时不回退到 root/当前目录：把用户带到错误的目录比不做任何
     *   事更糟（会让人误以为核对过了）；记录继续保留，等待用户自行移除。
     */
    function openUploadTargetDir(item) {
      if (!state.token || state.endingSession) return;
      if (!isCurrentUpload(item) || state.uploads.indexOf(item) < 0) return;
      // targetDir 在 addFiles 时一定写入；为空说明记录来源异常，宁可不动也不猜一个目录。
      if (item.targetDir == null || item.targetDir === '') return;
      var target = String(item.targetDir);
      loadDir(target, { reset: true });
    }

    /**
     * 手动移除一条已结算的结果待核对记录。只移除“这条记录”本身，不代表成功、失败或服务端已核对。
     * 之所以要求已结算：abort 抛错但请求可能仍在途时还占着并发槽，若允许移除会留下
     * 看不见的占位；这种情况应继续显示该行并保留“取消”入口，让用户能再次尝试终止。
     */
    function removeUploadRecord(item) {
      if (!item.finished || item.state !== 'uncertain') return;
      var index = state.uploads.indexOf(item);
      if (index >= 0) state.uploads.splice(index, 1);
      renderUploads();
      updateUploadSummary();
    }

    /**
     * 进度更新只触碰该行自己的节点；节点缺失说明该项已被清理，直接返回而不是重新创建。
     */
    function updateUploadNode(item) {
      var nodes = state.uploadNodes.get(item.id);
      if (!nodes) return;
      nodes.name.textContent = item.name;
      nodes.status.textContent = uploadStateText(item);
      nodes.status.classList.toggle('is-done', item.state === 'done');
      nodes.status.classList.toggle('is-error', item.state === 'error');
      nodes.li.classList.toggle('is-done', item.state === 'done');
      nodes.li.classList.toggle('is-error', item.state === 'error');
      nodes.li.classList.toggle('is-cancelled', item.state === 'cancelled');
      // 待核对既不是成功也不是失败，单独一档颜色，避免被误读为已提交。
      nodes.status.classList.toggle('is-uncertain', item.state === 'uncertain');
      nodes.li.classList.toggle('is-uncertain', item.state === 'uncertain');

      var percent = item.size > 0 ? Math.min(100, Math.floor((item.loaded / item.size) * 100)) : 0;
      if (item.state === 'done') percent = 100;
      nodes.fill.style.width = percent + '%';

      // 结果未确认时不能画一条满宽进度条：满条在视觉上等于“100% 完成”，
      // 会让用户把它读成已提交成功。这里把进度条切成“不确定”外观（斜纹/琥珀），
      // 并把辅助技术语义改成“结果未知”，只保留传输中的真实百分比。
      var uncertain = item.state === 'uncertain';
      nodes.li.classList.toggle('is-uncertain-bar', uncertain);
      if (uncertain) {
        // 满宽只是“传输曾走到 100%”，不代表已确认。ARIA 对不确定进度条的约定是
        // 省略 aria-valuenow（同时可给 aria-valuetext 描述状态），若保留 valuenow=100
        // 读屏会优先报出“100%”，与待核对语义直接冲突。因此这里移除数值、只留文字语义，
        // 状态回到数值型（传输中/已提交）时再重新写入。
        nodes.bar.removeAttribute('aria-valuenow');
        nodes.bar.setAttribute('aria-valuetext', t('uploadUncertain'));
        nodes.bar.classList.add('is-indeterminate');
      } else {
        nodes.bar.removeAttribute('aria-valuetext');
        nodes.bar.setAttribute('aria-valuenow', String(percent));
        nodes.bar.classList.remove('is-indeterminate');
      }
      if (item.state === 'done') nodes.bar.setAttribute('aria-valuetext', t('uploadSubmitted'));

      var cancellable = item.state === 'queued' || item.state === 'uploading' ||
        (item.abortUncertain && !item.finished);
      // 按钮文案随语言切换重算；只在创建时写一次会让切到英文后仍显示旧语言。
      nodes.cancel.textContent = t('cancel');
      setHidden(nodes.cancel, !cancellable);
      nodes.cancel.setAttribute('aria-label', t('cancel') + ': ' + item.name);
      // “查看目录”只属于待核对记录：已知结果（成功/失败/取消）的核对动作没有意义，
      // 继续显示反而暗示结果仍可变。
      var checkable = item.state === 'uncertain';
      nodes.checkFolder.textContent = t('viewTargetDir');
      setHidden(nodes.checkFolder, !checkable);
      nodes.checkFolder.setAttribute('aria-label',
        t('viewTargetDir') + ': ' + item.name + ' · ' + t('uploadUncertainHint'));
      nodes.checkFolder.setAttribute('title', t('uploadUncertainHint'));
      // 只有已结算的待核对记录才提供移除；仍在途的（例如 abort 抛错）继续显示并保留取消入口，
      // 避免移除后留下看不见的并发占位。
      var removable = item.finished && item.state === 'uncertain';
      // 文案说明“只移除本页记录、不删除文件”：用户看到“移除”容易以为会删掉服务器上的文件，
      // 这里的动作只是清掉页面上的这条待核对记录。主文案保持短按钮，完整说明放 title/aria。
      nodes.remove.textContent = t('removeRecord');
      setHidden(nodes.remove, !removable);
      nodes.remove.setAttribute('aria-label', t('removeRecord') + ': ' + item.name + ' · ' + t('removeRecordHint'));
      nodes.remove.setAttribute('title', t('removeRecordHint'));
    }

    /**
     * 先移除已不存在的项再补齐新增项，保持行顺序与 state.uploads 一致，避免取消/清除后行序错乱。
     */
    function renderUploads() {
      var present = new Set();
      for (var i = 0; i < state.uploads.length; i += 1) present.add(state.uploads[i].id);

      state.uploadNodes.forEach(function (nodes, id) {
        if (!present.has(id)) {
          nodes.li.remove();
          state.uploadNodes.delete(id);
        }
      });

      for (var j = 0; j < state.uploads.length; j += 1) {
        var item = state.uploads[j];
        if (!state.uploadNodes.has(item.id)) {
          state.uploadNodes.set(item.id, createUploadNode(item));
        }
        updateUploadNode(item);
      }

      setHidden(els.uploads, state.uploads.length === 0);

      var labels = {};
      var anyActive = false;
      var anyFinished = false;
      for (var k = 0; k < state.uploads.length; k += 1) {
        var current = state.uploads[k];
        labels[current.targetLabel] = true;
        if (current.state === 'queued' || current.state === 'uploading' ||
            (current.abortUncertain && !current.finished)) anyActive = true;
        // 待核对记录不会被“清除已完成”清掉，因此它不应让该按钮看起来可用。
        else if (current.state !== 'uncertain') anyFinished = true;
      }
      var labelKeys = Object.keys(labels);
      els.uploadsTitle.textContent = labelKeys.length > 1
        ? t('uploadMultipleTargets')
        : t('uploadTarget', { path: labelKeys[0] || updatePathLabel() });
      setHidden(els.cancelAll, !anyActive);
      setHidden(els.clearDone, !anyFinished);
      // 按钮文案不解释行为边界，用 title/aria 说明待核对记录会保留，避免用户以为被清掉。
      els.clearDone.setAttribute('title', t('clearDoneHint'));
      els.clearDone.setAttribute('aria-label', t('clearDone') + ': ' + t('clearDoneHint'));
    }

    /**
     * 汇总只统计真实提交数，并把“待核对”单独计数，避免把未知结果算成成功或失败。
     */
    function updateUploadSummary() {
      if (!state.uploads.length) {
        setHidden(els.uploadSummary, true);
        els.uploadSummary.textContent = '';
        return;
      }
      var totalSize = 0;
      var done = 0;
      var uncertain = 0;
      for (var i = 0; i < state.uploads.length; i += 1) {
        totalSize += state.uploads[i].size || 0;
        if (state.uploads[i].state === 'done') done += 1;
        else if (state.uploads[i].state === 'uncertain') uncertain += 1;
      }
      setHidden(els.uploadSummary, false);
      els.uploadSummary.textContent = t('uploadSummary', {
        count: state.uploads.length,
        size: formatSize(totalSize),
        done: done
      });
      // 有结果待核对的条目时追加一段提示，避免汇总里的“已提交 N”被当成全部成功。
      if (uncertain > 0) {
        els.uploadSummary.textContent += ' · ' + t('uploadSummaryUncertain', { count: uncertain });
      }
    }

    /* ---------------- 事件绑定 ---------------- */

    function bindEvents() {
      els.themeBtn.addEventListener('click', function () {
        applyTheme(themeCycle(state.theme));
      });

      els.langBtn.addEventListener('click', function () {
        applyLocale(state.locale === 'zh' ? 'en' : 'zh');
      });

      els.disconnectBtn.addEventListener('click', function () {
        endSession('disconnected');
      });

      els.connectRetry.addEventListener('click', function () {
        // deferred 状态下没有 pendingSecret，只有内存里的 newLinkSecret；
        // 这条分支把“使用此链接”落到真实配对，而不是一个点了没反应的按钮。
        if (state.newLinkSecret) pairWithNewLink();
        else if (state.pendingSecret) startPairing(state.pendingSecret);
        else if (state.token) loadDir(state.dir, { reset: true });
      });

      els.linkNoticeDismiss.addEventListener('click', function () {
        hideLinkNotice();
      });

      els.browseBannerAction.addEventListener('click', function () {
        state.checkFailures = 0;
        // 手动重连是明确的用户意图：绕过自动校验冷却，但仍在途时只排队一次。
        checkSession({ force: true });
        loadDir(state.dir, { reset: true });
      });

      els.uploadBtn.addEventListener('click', function () {
        if (!state.token) return;
        els.fileInput.click();
      });

      if (els.upBtn) {
        els.upBtn.addEventListener('click', function () {
          goUp();
        });
      }

      if (els.refreshBtn) {
        els.refreshBtn.addEventListener('click', function () {
          if (!state.token) return;
          loadDir(state.dir, { reset: true });
        });
      }

      els.fileInput.addEventListener('change', function () {
        if (els.fileInput.files && els.fileInput.files.length) addFiles(els.fileInput.files);
        els.fileInput.value = '';
      });

      els.cancelAll.addEventListener('click', function () {
        cancelAllUploads(false);
      });

      els.clearDone.addEventListener('click', function () {
        // 只清理已结算且结果明确的条目；待核对记录保留，避免无提示丢掉未知结果。
        state.uploads = state.uploads.filter(function (item) {
          return item.state === 'queued' || item.state === 'uploading' || item.state === 'uncertain';
        });
        renderUploads();
        updateUploadSummary();
      });

      els.listRetry.addEventListener('click', function () {
        // 按失败请求的原类型重发：协议错误或翻页失败时不能退化成“清空重取”，
        // 否则会丢掉已确认的列表，把一次可重试的协议错误变成目录看起来为空。
        retryListRequest();
      });

      els.loadMore.addEventListener('click', function () {
        loadDir(state.dir, { reset: false });
      });

      doc.addEventListener('visibilitychange', function () {
        // 恢复可见时只做一次轻量确认；快速切换标签会被最小间隔挡住，不会形成请求风暴。
        if (!doc.hidden) checkSession({ resume: true });
      });

      // 同页 fragment 变化（用户粘贴新分享链接后回车）不会重载页面，只触发 hashchange；
      // 没有这条监听时，断开状态下打开新链接会毫无反应，密钥还会残留在地址栏。
      win.addEventListener('hashchange', function () {
        handleHashChange();
      });

      doc.addEventListener('dragenter', function (event) {
        if (!hasFiles(event)) return;
        event.preventDefault();
        dragDepth += 1;
        if (state.token) setHidden(els.dropOverlay, false);
      });

      doc.addEventListener('dragover', function (event) {
        if (hasFiles(event)) {
          event.preventDefault();
          if (event.dataTransfer) event.dataTransfer.dropEffect = 'copy';
          return;
        }
        // 非文件拖放（链接、选中文本）不解释其目录语义，但也不让浏览器把页面导航走，
        // 否则一次误拖就会丢掉当前会话。只吞掉默认导航，不做任何上传或跳转。
        if (state.token) event.preventDefault();
      });

      doc.addEventListener('dragleave', function () {
        // dragleave 在部分浏览器上 types 为空；这里不依赖 types，直接按深度收敛，
        // 否则遮罩会一直停在屏幕上挡住页面。
        dragDepth = Math.max(0, dragDepth - 1);
        if (dragDepth === 0) setHidden(els.dropOverlay, true);
      });

      win.addEventListener('blur', function () {
        dragDepth = 0;
        setHidden(els.dropOverlay, true);
      });

      doc.addEventListener('drop', function (event) {
        if (!hasFiles(event)) {
          // 同 dragover：不猜测拖入内容的目录含义，只阻止浏览器用拖入的链接替换当前页面。
          if (state.token) event.preventDefault();
          return;
        }
        event.preventDefault();
        dragDepth = 0;
        setHidden(els.dropOverlay, true);
        if (!state.token) return;
        var files = event.dataTransfer ? event.dataTransfer.files : null;
        if (files && files.length) addFiles(files);
      });

      doc.addEventListener('keydown', function (event) {
        if (event.key === 'Escape') setHidden(els.dropOverlay, true);
        if (event.altKey && !event.ctrlKey && !event.metaKey && !event.shiftKey && event.key === 'ArrowUp') {
          if (state.token && state.breadcrumbs.length >= 2) {
            if (typeof event.preventDefault === 'function') event.preventDefault();
            goUp();
          }
        }
      });
    }

    /**
     * 拖拽事件里只有 types 包含 Files 才是文件拖入；据此过滤掉选中文本等其它拖拽，防止遮罩被误触发。
     */
    function hasFiles(event) {
      var types = event && event.dataTransfer ? event.dataTransfer.types : null;
      if (!types) return false;
      for (var i = 0; i < types.length; i += 1) {
        if (types[i] === 'Files') return true;
      }
      return false;
    }

    /* ---------------- 启动 ---------------- */

    var storedTheme = readStored('lan-transfer.theme');
    applyTheme(storedTheme === 'light' || storedTheme === 'dark' ? storedTheme : 'auto');

    var storedLocale = readStored('lan-transfer.lang');
    applyLocale(storedLocale === 'en' || storedLocale === 'zh'
      ? storedLocale
      : localeFromNavigator(win.navigator ? win.navigator.language : ''));

    bindEvents();

    // 片段在任何网络请求前移除：它只用于配对，不应留在地址栏、历史记录或转发内容里。
    var rawHash = win.location.hash || '';
    var pairing = parsePairingHash(rawHash);
    if (rawHash) {
      try {
        win.history.replaceState(null, '', win.location.pathname + win.location.search);
      } catch (error) {
        win.location.hash = '';
      }
    }

    if (pairing.ok) {
      startPairing(pairing.secret);
    } else {
      showConnect('missing');
    }
  }

  return {
    __boot: bootPage,
    parsePairingHash: parsePairingHash,
    buildListUrl: buildListUrl,
    buildUploadUrl: buildUploadUrl,
    parseListPayload: parseListPayload,
    listTargetMatches: listTargetMatches,
    mergeEntries: mergeEntries,
    formatSize: formatSize,
    formatModified: formatModified,
    formatRemaining: formatRemaining,
    isSafeTicketUrl: isSafeTicketUrl,
    describeApiError: describeApiError,
    validatePairResponse: validatePairResponse,
    themeCycle: themeCycle,
    localeFromNavigator: localeFromNavigator,
    translate: translate
  };
});
