#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""真实 Sidecar 黑盒回归：stdio JSON-RPC 握手 + 真实 HTTP 传输。
@author kongweiguang

为什么需要这个脚本：
- 现有 `tests/backend/*.rs` 用内存双向流和 axum 的 `oneshot` 驱动路由，能证明中间件与
  处理器契约，但不覆盖「真实子进程、真实标准输入输出、真实内核监听端口、真实进程退出」。
- `web/*.test.js` 只覆盖浏览器端纯逻辑，不能证明 Sidecar 真的按这套协议收发字节。
- 本脚本只启动调用方显式传入的 Sidecar 二进制，用系统分配的 127.0.0.1 端口和合成文件
  完成一次端到端联调，从而填补「单测/fake DOM」与「真实宿主/局域网」之间的缺口。

覆盖范围：
- NDJSON 握手（`initialize`/`initialized`）、`view.snapshot`/`view.open`/`view.action`；
- 配对、会话校验、目录列表、真实上传（含中文名、同名不覆盖、多文件）与字节回读；
- 下载票据、GET/HEAD 整份字节、计划 5.6/5.8 的 Range 语义（单区间、后缀、开区间、
  超范围 416、未知范围单位忽略）、非法 Origin/Host 拒绝、路径逃逸拒绝；
- 客户端中断上传：等到服务端暂存可见（Windows 为命名暂存）或等待窗口结束后再断开，
  随后验证暂存清理与最终文件不落盘；Linux 匿名暂存不可见时降级为只验证无落盘无残留；
- 并发中断上传后仍有可用上传占位且服务未被卡死；这能证伪「全部占位泄漏」，
  但服务端不暴露占位计数，不能证明逐请求归还；
- 停止后监听端口不再可达；重启后旧令牌/旧票据失效；`shutdown` 后进程退出并回收。

明确未覆盖：
- 真实浏览器、真实局域网、GPUI 宿主、安装包与多平台原生验收；
- 服务端主动取消上传（公开 HTTP 契约没有该接口）；
- 并发上传占位的逐请求归还核算（无法无侵入观测，只能做上述证伪式检查）；
- 非 Windows 命名暂存平台上的「暂存确实已创建」证明：Linux 匿名暂存没有目录项，
  脚本只能缩短等待后验证「无最终文件、无可见残留」，不能声称清理了已观察到的暂存；
- HEAD 携带 Range 的独立用例（当前实现 GET/HEAD 共用处理器；最终契约未单列时补测
  价值有限，待契约明确后再加）；
- 超时停止、监听任务 panic 等只能由故障注入覆盖的路径。

契约状态（必须与最终实现一起核对）：
- 下载票据支持浏览器单区间 Range：可满足区间返回 206 与 `Content-Range`，起点越界
  返回 416 与 `bytes */size`；语法非法、未知单位或多区间按 RFC 7233 忽略并整份返回
  200。与 README/SECURITY 及 `src/http/download.rs` 当前声明一致；若实现退回
  `Accept-Ranges: none` 或把可忽略的 Range 判成 416，本脚本明确失败。

安全约束：
- 只使用新建临时根目录；CWD、TMP/TEMP、data_dir 都指向该临时根；子进程使用最小
  必要环境变量，不整体继承用户环境，HOME/USERPROFILE/APPDATA/LOCALAPPDATA/XDG_*
  等用户目录指针全部指向隔离目录，用户凭据类变量不传入被测二进制；
- 配对密钥与 Bearer 令牌只保存在进程内，不打印、不写入断言消息，断言失败时也只
  输出固定说明文本，不回显原值；
- 每个 RPC/HTTP 步骤和整个子进程都有明确时间上限；失败也会终止并回收本脚本创建的进程；
- 必须用 `python -I` 运行，且 CWD 不在被测目录，避免从被测目录加载模块。
"""

import argparse
import http.client
import itertools
import json
import os
import queue
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
from pathlib import Path

PLUGIN_ID = "kongweiguang.lan-transfer"
PROTOCOL_VERSION = 1
# 与宿主当前权威契约一致：插件清单声明 `api: ">=1.1.0 <2.0.0"`，握手 host_api 取 1.1.0。
# 覆盖更旧宿主版本的握手属于宿主侧兼容性回归，不在本脚本范围。
HOST_API_VERSION = "1.1.0"

HANDSHAKE_TIMEOUT = 15.0
RPC_TIMEOUT = 15.0
HTTP_TIMEOUT = 20.0
STOP_TIMEOUT = 10.0
EXIT_TIMEOUT = 10.0
ABORT_CLEANUP_TIMEOUT = 10.0
# 等待服务端暂存文件出现的窗口；Windows 命名暂存可观察，Linux 匿名暂存永不出现。
ABORT_STAGING_APPEAR_TIMEOUT = 5.0

ARGS = None


def _redact(text):
    """把长随机串替换掉：错误输出可能来自子进程，不能把令牌或配对密钥带进日志。"""
    return re.sub(r"[A-Za-z0-9_\-]{32,}", "<redacted>", text)


def _safe_body(value, limit=400):
    """限制失败信息长度并脱敏，避免断言失败时把大响应或凭据刷进终端。"""
    if isinstance(value, (bytes, bytearray)):
        text = bytes(value[:limit]).decode("utf-8", "replace")
    else:
        text = json.dumps(value, ensure_ascii=False) if not isinstance(value, str) else value
    text = _redact(text)
    return text[:limit]


def _parse_link(link):
    """从启动回执的分享链接里取出回环地址、端口与 fragment 密钥。

    只接受 `http://127.0.0.1:<port>/#<secret>`：脚本把服务固定绑定回环，任何其他主机
    都说明实现偏离了本测试的隔离前提，应当直接失败而不是继续访问。
    """
    if not isinstance(link, str):
        raise AssertionError("link is not a string")
    parts = urllib.parse.urlsplit(link)
    if parts.scheme != "http" or parts.hostname != "127.0.0.1":
        raise AssertionError("link is not loopback http")
    try:
        port = parts.port
    except ValueError as error:
        raise AssertionError("link has an invalid port") from error
    if port is None or parts.path not in ("", "/"):
        raise AssertionError("link has no port or unexpected path")
    if not parts.fragment:
        raise AssertionError("link has no pairing fragment")
    return "127.0.0.1", port, parts.fragment


def _percent(value):
    """按 RFC 3986 对查询参数做 UTF-8 百分号编码，与服务端 name/fileId 契约一致。"""
    return urllib.parse.quote(value, safe="")


def _header(headers, name):
    """HTTP 头名大小写不敏感；http.client 返回的键保留服务端拼写。"""
    lowered = name.lower()
    for key, value in headers.items():
        if key.lower() == lowered:
            return value
    return None


def _json_bytes(value):
    """请求体统一用紧凑 UTF-8 JSON，避免依赖终端或系统代码页。"""
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


class RpcClient:
    """驱动真实 Sidecar 子进程的 NDJSON 客户端。

    标准输出由独立线程持续读取：Windows 的管道不能用 select，主线程阻塞等待时
    还必须同时排空标准错误，否则子进程日志写满管道会反向卡住协议循环。
    """

    def __init__(self, binary, cwd, env, verbose=False):
        """只保存启动所需材料，不在构造阶段拉起进程。

        构造与启动分开，是因为调用方需要在 `setUp` 里先登记清理钩子再启动；若构造函数
        直接启动子进程，登记失败时就会留下无人回收的进程。
        """
        self.binary = str(binary)
        self.cwd = str(cwd)
        self.env = env
        self.verbose = verbose
        self.proc = None
        self.exited = False
        self._queue = queue.Queue()
        self._stderr = []
        self._stdout_thread = None
        self._stderr_thread = None
        self._write_lock = threading.Lock()
        self._ids = itertools.count(1)

    def start(self):
        """启动子进程并开始排空两条管道；失败时不留半启动状态。"""
        flags = getattr(subprocess, "CREATE_NO_WINDOW", 0) if os.name == "nt" else 0
        self.proc = subprocess.Popen(
            [self.binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=self.cwd,
            env=self.env,
            # 保留 Python 默认缓冲：标准输出逐行解析若用无缓冲原始流会退化成逐字节
            # 系统调用；标准输入每次写完都显式 flush，不会让请求帧滞留在缓冲区。
            creationflags=flags,
        )
        self._stdout_thread = threading.Thread(target=self._pump_stdout, daemon=True)
        self._stderr_thread = threading.Thread(target=self._pump_stderr, daemon=True)
        self._stdout_thread.start()
        self._stderr_thread.start()

    def _pump_stdout(self):
        """逐行解析协议帧；非法帧也入队，让等待方看到明确错误而不是超时。"""
        try:
            while True:
                raw = self.proc.stdout.readline()
                if not raw:
                    break
                line = raw.decode("utf-8", "replace").strip()
                if not line:
                    continue
                try:
                    message = json.loads(line)
                except json.JSONDecodeError:
                    message = {"__invalid__": line}
                self._queue.put(message)
        except (OSError, ValueError):
            # 清理阶段关闭管道会让阻塞中的 readline 抛错；这是预期收尾，不是协议故障，
            # 必须吞掉以免线程回溯污染测试输出。
            pass
        finally:
            self._queue.put(None)

    def _pump_stderr(self):
        """保留有限行数的标准错误用于诊断，并脱敏后输出，防止凭据进入日志。"""
        try:
            while True:
                raw = self.proc.stderr.readline()
                if not raw:
                    break
                line = _redact(raw.decode("utf-8", "replace").rstrip())
                if line:
                    self._stderr.append(line)
                    del self._stderr[:-200]
                    if self.verbose:
                        print(f"[sidecar stderr] {line}", file=sys.stderr)
        except (OSError, ValueError):
            pass

    def _write(self, frame):
        """写入一行协议帧；管道关闭时返回失败，由调用方转成可读断言。"""
        data = json.dumps(frame, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n"
        with self._write_lock:
            try:
                self.proc.stdin.write(data)
                self.proc.stdin.flush()
            except (BrokenPipeError, OSError) as error:
                raise AssertionError(f"sidecar stdin closed: {error}") from error

    def request(self, method, params, timeout):
        """发送请求并按 id 等待回执；通知帧与迟到帧一律跳过，避免误配。

        宿主发往插件的请求 ID 必须以 `h:` 开头：SDK 的 `RpcRequest::validate`
        按发起方校验前缀，用别的命名会在握手前就被判成 invalid request。
        """
        request_id = f"h:{next(self._ids)}"
        self._write({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        end = time.monotonic() + timeout
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"RPC {method} timed out; stderr tail: {self.stderr_tail()}")
            try:
                message = self._queue.get(timeout=remaining)
            except queue.Empty:
                raise AssertionError(f"RPC {method} timed out; stderr tail: {self.stderr_tail()}")
            if message is None:
                raise AssertionError(
                    f"sidecar stdout closed before replying to {method}; "
                    f"stderr tail: {self.stderr_tail()}"
                )
            if message.get("id") == request_id:
                return message
            if "__invalid__" in message:
                raise AssertionError(f"invalid NDJSON frame: {_safe_body(message['__invalid__'])}")

    def notify(self, method, params):
        """通知没有回执；用于 `initialized`，失败与请求同样视为协议断开。"""
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def wait_exit(self, timeout):
        """等待进程真正退出并返回退出码；超时说明 `shutdown` 后没有回收。"""
        try:
            code = self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            raise AssertionError(
                f"sidecar did not exit within {timeout}s; stderr tail: {self.stderr_tail()}"
            ) from error
        self.exited = True
        return code

    def stderr_tail(self):
        """只给最后几行诊断，且已经脱敏。"""
        return " | ".join(self._stderr[-5:])

    def close(self):
        """确保本脚本创建的进程被回收；返回是否已确认退出。

        分三档升级（等待 → terminate → kill），每一档都有上限，但操作系统调度与
        不可中断的系统调用不受本脚本控制，因此这里不承诺绝对墙钟时间：最后一档
        超时即返回 False，由调用方显式失败并保留诊断材料，绝不静默当作已回收。
        """
        if self.proc is None:
            return True
        try:
            if self.proc.stdin is not None and not self.proc.stdin.closed:
                self.proc.stdin.close()
        except (OSError, ValueError):
            pass
        if self.proc.poll() is None:
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    try:
                        self.proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        pass
        confirmed = self.proc.poll() is not None
        self.exited = confirmed
        for thread in (self._stdout_thread, self._stderr_thread):
            if thread is not None:
                thread.join(timeout=2)
        for stream in (self.proc.stdout, self.proc.stderr):
            try:
                if stream is not None:
                    stream.close()
            except (OSError, ValueError):
                pass
        return confirmed


class HttpApi:
    """最小 HTTP 客户端；每次请求独立连接，避免复用掩盖停止后的端口状态。"""

    def __init__(self, port, timeout=HTTP_TIMEOUT):
        """固定回环地址与同源 Origin；端口来自真实启动回执，不自行猜测。"""
        self.port = port
        self.origin = f"http://127.0.0.1:{port}"
        self.timeout = timeout

    def request(self, method, path, headers=None, body=None):
        """发请求并读完响应；Host 默认与实际连接地址一致，便于服务端精确同源校验。"""
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=self.timeout)
        try:
            merged = {"Host": f"127.0.0.1:{self.port}"}
            if headers:
                merged.update(headers)
            conn.request(method, path, body=body, headers=merged)
            response = conn.getresponse()
            data = response.read()
            return response.status, dict(response.getheaders()), data
        finally:
            conn.close()

    def pair(self, secret, origin=None, include_origin=True):
        """配对必须带同源 Origin；调用方可以显式覆盖或省略以验证拒绝分支。"""
        headers = {"Content-Type": "application/json"}
        if include_origin:
            headers["Origin"] = self.origin if origin is None else origin
        return self.request("POST", "/api/pair", headers, _json_bytes({"secret": secret}))

    def upload(self, token, name, payload, dir_id="root", origin=None, include_origin=True):
        """上传是原始字节体；文件名只走查询串，与服务端不覆盖提交语义一致。"""
        headers = {
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/octet-stream",
        }
        if include_origin:
            headers["Origin"] = self.origin if origin is None else origin
        path = f"/api/upload?dir={_percent(dir_id)}&name={_percent(name)}"
        return self.request("POST", path, headers, payload)


class HelperSelfCheck(unittest.TestCase):
    """不启动二进制的脚本自检；确保真正联调时失败原因来自 Sidecar 而不是辅助函数。"""

    def test_parse_link_shape(self):
        """标准链接必须解析出端口与密钥；这是启动断言的基础形状。"""
        host, port, secret = _parse_link("http://127.0.0.1:43210/#abc123")
        self.assertEqual(host, "127.0.0.1")
        self.assertEqual(port, 43210)
        self.assertEqual(secret, "abc123")

    def test_parse_link_rejects_other_hosts(self):
        """非回环、HTTPS、缺 fragment 都必须拒绝：解析器放行即破坏脚本隔离前提。"""
        for link in [
            "http://localhost:1/#x",
            "https://127.0.0.1:1/#x",
            "http://127.0.0.1:1/",
            "http://127.0.0.1:1/#",
        ]:
            with self.assertRaises(AssertionError):
                _parse_link(link)

    def test_percent_encoding_matches_contract(self):
        """中文与空格必须被编码、安全字符保持原样：与查询串契约对齐的基础校验。"""
        encoded = _percent("报告 2026.txt")
        self.assertNotIn(" ", encoded)
        self.assertTrue(encoded.startswith("%E6%8A%A5"), encoded)
        self.assertEqual(_percent("a-b_c.txt"), "a-b_c.txt")

    def test_redaction_hides_long_tokens(self):
        """长随机串必须被替换：保证真实联调失败时令牌不会进入终端输出。"""
        token = "A" * 43
        self.assertNotIn(token, _redact(f"token={token}"))
        self.assertIn("<redacted>", _redact(token))

    def test_manifest_identity(self):
        """清单身必须与脚本常量一致；缺失清单时跳过，不猜版本导致假联调。"""
        if ARGS is None or not ARGS.manifest or not Path(ARGS.manifest).is_file():
            self.skipTest("未找到插件清单")
        manifest = json.loads(Path(ARGS.manifest).read_text(encoding="utf-8"))
        self.assertEqual(manifest.get("id"), PLUGIN_ID)
        self.assertIsInstance(manifest.get("version"), str)
        self.assertTrue(manifest["version"])


class SidecarEndToEnd(unittest.TestCase):
    """一次完整端到端流程；所有步骤共享同一个真实子进程，失败时清理也覆盖它。"""

    def setUp(self):
        """建立隔离环境并完成真实握手；每一步失败都已有可回滚的清理登记。

        临时根创建后立即登记清理，随后才建子目录、读清单、启动进程：这些后续步骤
        都可能失败，若把登记拖到最后，失败路径就会把临时数据留在磁盘上。
        """
        if ARGS is None or ARGS.sidecar is None:
            self.skipTest("未提供 --sidecar，跳过真实 Sidecar 联调")
        binary = Path(ARGS.sidecar)
        if not binary.is_file():
            self.fail(f"Sidecar 二进制不存在：{binary}")

        self.deadline = time.monotonic() + ARGS.timeout
        # 临时根创建成功后立刻登记清理，再继续建子目录和读清单：这两步都可能因
        # 权限、磁盘或清单损坏失败，若等到最后才登记就会把临时数据留在磁盘上。
        self.root = Path(tempfile.mkdtemp(prefix="klt-sidecar-e2e-"))
        self._preserve_root = False
        self.addCleanup(self._cleanup_root)
        self.share = self.root / "share"
        self.work = self.root / "work"
        self.data = self.root / "data"
        self.tmp = self.root / "tmp"
        self.cfg = self.root / "cfg"
        for directory in (self.share, self.work, self.data, self.tmp, self.cfg):
            directory.mkdir(parents=True, exist_ok=True)

        # 合成文件只用于真实传输比对；目录名不以点开头，避免被共享规则过滤。
        (self.share / "docs").mkdir()
        (self.share / "docs" / "nested.txt").write_bytes(b"nested\n")
        (self.share / "alpha.txt").write_bytes(b"alpha initial\n")
        (self.share / "beta.bin").write_bytes(bytes(range(256)) * 4)

        # 最小必要环境：不整体复制用户环境，防止宿主环境里的凭据/代理等变量流入被测
        # 二进制；再按平台补齐运行必需的系统变量与隔离的用户目录指针。必需变量缺失
        # 时显式列出并失败，而不是静默以残缺环境启动。
        required = ["SYSTEMROOT", "WINDIR"] if os.name == "nt" else ["PATH"]
        missing = [name for name in required if not os.environ.get(name)]
        if missing:
            self.fail(f"宿主缺少被测二进制运行必需的环境变量：{missing}")
        env = {
            "PATH": os.environ.get("PATH", ""),
            "SYSTEMROOT": os.environ["SYSTEMROOT"] if os.name == "nt" else "",
            "COMSPEC": os.environ.get("COMSPEC", ""),
            "LANG": "C.UTF-8",
            "TMP": str(self.tmp),
            "TEMP": str(self.tmp),
            "TMPDIR": str(self.tmp),
            # Sidecar 当前不读配置根；显式指向临时目录，防止未来接入后误用用户配置。
            "KERMINAL_CONFIG_ROOT": str(self.cfg),
        }
        if os.name != "nt":
            env.pop("SYSTEMROOT", None)
            env["HOME"] = str(self.data)
        else:
            # 可能影响应用配置/缓存的用户目录指针全部指向隔离目录。
            env["USERPROFILE"] = str(self.data)
            env["APPDATA"] = str(self.data)
            env["LOCALAPPDATA"] = str(self.data)
        for extra in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"):
            env[extra] = str(self.data)

        self.manifest = self._load_manifest()
        self.plugin_id = (self.manifest or {}).get("id", PLUGIN_ID)
        self.plugin_version = ARGS.plugin_version or (self.manifest or {}).get("version")
        if not self.plugin_version:
            self.fail("无法确定插件版本：请提供 --plugin-version 或可读的插件清单")

        self.rpc = RpcClient(binary, self.work, env, ARGS.verbose)
        # 注册顺序决定执行顺序（LIFO）：进程回收最先执行，看门狗其次，临时目录最后。
        # 进程未确认退出时目录清理被跳过并显式失败，路径留给人工排查；不允许
        # 「删除失败但测试仍绿」的静默路径。
        self.addCleanup(self._cleanup_process)
        # 看门狗兜底：单个步骤已有超时，但主线程若卡在不支持超时的系统调用里，
        # 仍必须保证子进程不会超出整体预算继续监听；宽限只留给正常回收流程。
        self._watchdog = threading.Timer(ARGS.timeout + 10.0, self._kill_stray_process)
        self._watchdog.daemon = True
        self.addCleanup(self._cleanup_watchdog)
        self._watchdog.start()
        self.rpc.start()

        self._phase("handshake")
        reply = self.rpc.request(
            "initialize",
            {
                "protocol": PROTOCOL_VERSION,
                "host_api": HOST_API_VERSION,
                "plugin": {"id": self.plugin_id, "version": self.plugin_version},
                "locale": "en-US",
                "theme": "light",
                "data_dir": str(self.data),
                "generation": 1,
            },
            HANDSHAKE_TIMEOUT,
        )
        self.assertIsNone(
            reply.get("error"),
            "initialize 失败；若二进制用 KERMINAL_PLUGIN_VERSION 构建，请传 --plugin-version",
        )
        result = reply["result"]
        self.assertEqual(result["protocol"], PROTOCOL_VERSION)
        self.assertEqual(result["plugin"]["id"], self.plugin_id)
        self.assertEqual(result["plugin"]["version"], self.plugin_version)
        self.rpc.notify("initialized", {})

        self.token = None
        self.http = None

    def _load_manifest(self):
        """清单只作身份数据读取；缺失时由 --plugin-version 兜底，不猜构建版本。"""
        if ARGS.manifest and Path(ARGS.manifest).is_file():
            return json.loads(Path(ARGS.manifest).read_text(encoding="utf-8"))
        return None

    def _kill_stray_process(self):
        """看门狗只在整体预算被突破时强制终止子进程，防止测试挂起后留下监听端口。

        它不负责断言结果：正常路径由清理与退出码断言覆盖；这里仅做最后一道资源兜底，
        因此不动临时目录——进程退出前删除目录会失败，路径应留给 `_cleanup_root`。
        """
        proc = getattr(getattr(self, "rpc", None), "proc", None)
        if proc is not None and proc.poll() is None:
            proc.kill()

    def _cleanup_process(self):
        """先于目录清理执行的进程回收；无法确认退出时显式失败而不是静默继续。

        失败时保留临时根（`_preserve_root`）与标准错误尾部：强杀后仍不退出属于
        必须人工排查的异常，删除诊断材料会让失败原因不可知。
        """
        watchdog = getattr(self, "_watchdog", None)
        if watchdog is not None:
            watchdog.cancel()
        rpc = getattr(self, "rpc", None)
        if rpc is None:
            return
        if not rpc.close():
            self._preserve_root = True
            self.fail(
                "无法确认 Sidecar 进程已退出（等待/terminate/kill 均超时）；"
                f"临时根已保留：{self.root}；stderr 尾部：{rpc.stderr_tail()}"
            )

    def _cleanup_watchdog(self):
        """看门狗是普通线程，正常回收即可；单独成方法是为了让注册顺序一目了然。"""
        watchdog = getattr(self, "_watchdog", None)
        if watchdog is not None:
            watchdog.cancel()

    def _cleanup_root(self):
        """删除本次创建的临时根；只在进程已确认退出后执行，失败必须让测试变红。

        `ignore_errors=True` 会把「文件被占用」吞成伪成功，这里显式重试有限次数
        （Windows 句柄释放可能有短暂延迟），仍失败则保留路径并报告：宁可留下
        一个可诊断的临时目录，也不能谎报清理完成。清理目标始终是本脚本
        `mkdtemp` 创建的路径，不涉及任何脚本外文件。
        """
        root = getattr(self, "root", None)
        if root is None:
            return
        if getattr(self, "_preserve_root", False):
            print(f"[e2e] 保留临时根供排查：{root}", file=sys.stderr)
            return
        for attempt in range(10):
            try:
                shutil.rmtree(root)
                return
            except FileNotFoundError:
                return
            except OSError:
                time.sleep(0.2)
        try:
            leftover = os.listdir(root)
        except OSError:
            leftover = ["<unreadable>"]
        self.fail(f"临时根清理失败，已保留供排查：{root}；残留条目：{leftover[:10]}")

    def _phase(self, name):
        """每个阶段先检查整体截止时间，避免某个步骤超时后继续无限推进。"""
        if time.monotonic() > self.deadline:
            self.fail(f"整体时间上限 {ARGS.timeout}s 已耗尽，停在阶段：{name}")
        if ARGS.verbose:
            print(f"[e2e] {name}", file=sys.stderr)

    def _expect_ok(self, reply, label):
        """统一断言 RPC 无错误，并返回 result；错误消息只含脱敏后的短文本。"""
        self.assertIsNone(reply.get("error"), f"{label} 失败：{_safe_body(reply.get('error'))}")
        return reply.get("result", {})

    def _json(self, status, data, label, expected=200):
        """断言状态并解析 JSON；避免在失败信息里输出令牌响应体。"""
        self.assertEqual(status, expected, f"{label} 状态码不符")
        try:
            return json.loads(data.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            # 用 raise 而不是 self.fail：需要保留原始解析异常作为因果链，便于定位是
            # 编码问题还是响应体被截断。
            raise AssertionError(f"{label} 响应不是合法 JSON：{_safe_body(data)}") from error

    def _wait_port_closed(self, port, timeout=STOP_TIMEOUT):
        """轮询直到端口拒绝连接；停止回执只代表任务已确认退出，这里再验证内核状态。"""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                    pass
            except OSError:
                return
            time.sleep(0.1)
        self.fail(f"停止后端口 {port} 仍在接受连接")

    def _assert_no_staging(self):
        """验证共享根目录没有留下可见的内部暂存项；这是平台可观察性的下限检查。

        只覆盖本实现当前可观察的形态：Windows/Unix 回退的命名暂存（`TEMP_PREFIX`
        即 `.klt-tmp-` 前缀）与隐藏文件。Linux 主路径是 `O_TMPFILE` 匿名暂存，没有
        目录项，任何目录枚举都看不到；它由进程退出间接回收，本断言不声称能证明
        匿名暂存已被清理。若未来实现改名暂存前缀或引入其他暂存材料，这里需要随
        实现同步，不能当作全量回收的证明。
        """
        leftovers = [name for name in os.listdir(self.share) if name.startswith(".klt-tmp-")]
        self.assertEqual(leftovers, [], "存在未清理的暂存文件")

    def test_real_sidecar_http_flow(self):
        """按真实宿主/浏览器的调用顺序走完整链路，逐步断言协议与字节结果。"""
        self._phase("view.snapshot")
        result = self._expect_ok(
            self.rpc.request("view.snapshot", {}, RPC_TIMEOUT), "view.snapshot"
        )
        self.assertEqual(result["state_version"], 1)
        # 恢复态只允许非敏感字段：链接、二维码与令牌不能回到宿主草稿。
        self.assertEqual(set(result["state"].keys()), {"selected_address", "root_path"})
        self.assertIsInstance(result["state"]["selected_address"], str)
        self.assertTrue(result["state"]["selected_address"])

        self._phase("view.open")
        result = self._expect_ok(
            self.rpc.request(
                "view.open",
                {"view": "lan.transfer.panel", "instance": "e2e", "scope": {}},
                RPC_TIMEOUT,
            ),
            "view.open",
        )
        self.assertFalse(result["data"]["running"])
        self.assertIsInstance(result["data"]["stats"], str)

        self._phase("start service")
        result = self._expect_ok(
            self.rpc.request(
                "view.action",
                {
                    "instance": "e2e",
                    "action": "start",
                    "data": {
                        "root_path": str(self.share),
                        "selected_address": "127.0.0.1",
                    },
                },
                RPC_TIMEOUT,
            ),
            "start",
        )
        patch = result["patch"]
        self.assertTrue(patch["running"])
        self.assertEqual(patch["selected_address"], "127.0.0.1")
        self.assertIsInstance(patch["address"], str)
        _, port, secret = _parse_link(patch["link"])
        self.assertEqual(patch["address"], f"127.0.0.1:{port}")
        self.http = HttpApi(port)

        self._phase("page headers")
        status, headers, body = self.http.request("GET", "/")
        self.assertEqual(status, 200)
        self.assertIn("frame-ancestors 'none'", _header(headers, "Content-Security-Policy") or "")
        self.assertEqual(_header(headers, "X-Frame-Options"), "DENY")
        self.assertIn(b"<!doctype html", body.lower())

        self._phase("origin rejection")
        status, _, _ = self.http.pair(secret, origin="http://attacker.example")
        self.assertEqual(status, 403, "跨站 Origin 的配对必须拒绝")
        status, _, _ = self.http.pair(secret, include_origin=False)
        self.assertEqual(status, 403, "缺少 Origin 的配对必须拒绝")

        self._phase("pairing")
        status, _, data = self.http.pair(secret)
        payload = self._json(status, data, "pair")
        self.assertIsInstance(payload.get("token"), str)
        self.assertGreaterEqual(len(payload["token"]), 16)
        self.assertIsInstance(payload.get("expiresAt"), int)
        self.token = payload["token"]

        self._phase("session")
        status, _, data = self.http.request(
            "GET", "/api/session", {"Authorization": f"Bearer {self.token}"}
        )
        payload = self._json(status, data, "session")
        self.assertIsInstance(payload.get("expiresAt"), int)

        self._phase("authorization")
        status, _, data = self.http.request("GET", "/api/list?dir=root")
        payload = self._json(status, data, "list without token", expected=401)
        self.assertEqual(payload["error"]["code"], "unauthorized")

        self._phase("list")
        status, _, data = self.http.request(
            "GET", "/api/list?dir=root", {"Authorization": f"Bearer {self.token}"}
        )
        payload = self._json(status, data, "list")
        self.assertEqual(payload["breadcrumbs"][0]["id"], "root")
        names = {entry["name"]: entry for entry in payload["entries"]}
        self.assertEqual(names["docs"]["kind"], "directory")
        self.assertEqual(names["alpha.txt"]["kind"], "file")
        self.assertEqual(names["alpha.txt"]["size"], len(b"alpha initial\n"))
        self.assertIsNone(payload["nextCursor"])
        # 目录在前：排序契约由服务端保证，客户端不能自行重排破坏游标语义。
        self.assertEqual(payload["entries"][0]["name"], "docs")

        self._phase("upload payload")
        payload_bytes = bytes(range(256)) * 1024
        status, _, data = self.http.upload(self.token, "payload.bin", payload_bytes)
        uploaded = self._json(status, data, "upload")
        self.assertEqual(uploaded["name"], "payload.bin")
        self.assertEqual(uploaded["size"], len(payload_bytes))
        self.assertTrue((self.share / "payload.bin").read_bytes() == payload_bytes, "落盘字节不一致")

        self._phase("upload utf8 name")
        status, _, data = self.http.upload(self.token, "报告 2026.txt", b"utf8-name\n")
        uploaded = self._json(status, data, "upload utf8")
        self.assertEqual(uploaded["name"], "报告 2026.txt")
        self.assertTrue((self.share / "报告 2026.txt").is_file())

        self._phase("upload duplicate")
        for expected, content in (("dup.txt", b"first"), ("dup (1).txt", b"second")):
            status, _, data = self.http.upload(self.token, "dup.txt", content)
            uploaded = self._json(status, data, "upload duplicate")
            self.assertEqual(uploaded["name"], expected)
        self.assertTrue((self.share / "dup.txt").read_bytes() == b"first", "原文件被覆盖")
        self.assertTrue((self.share / "dup (1).txt").read_bytes() == b"second", "改名文件内容不符")

        self._phase("upload multiple files")
        for name, content in (("multi-a.bin", b"A" * 1000), ("multi-b.bin", b"B" * 2000)):
            status, _, data = self.http.upload(self.token, name, content)
            uploaded = self._json(status, data, "upload multi")
            self.assertEqual(uploaded["name"], name)

        self._phase("download ticket")
        status, _, data = self.http.request(
            "POST",
            "/api/download-ticket",
            {
                "Authorization": f"Bearer {self.token}",
                "Origin": self.http.origin,
                "Content-Type": "application/json",
            },
            _json_bytes({"fileId": "root/payload.bin"}),
        )
        ticket = self._json(status, data, "ticket")
        self.assertTrue(ticket["url"].startswith("/download/"))
        old_ticket_url = ticket["url"]

        self._phase("download bytes")
        status, headers, data = self.http.request("GET", old_ticket_url)
        self.assertEqual(status, 200)
        self.assertTrue(data == payload_bytes, f"下载字节不一致：{len(data)} vs {len(payload_bytes)}")
        self.assertEqual(_header(headers, "Content-Length"), str(len(payload_bytes)))
        self.assertIn("attachment", _header(headers, "Content-Disposition") or "")
        # 计划 5.6/5.8：下载票据必须支持浏览器 Range。当前 README/SECURITY 仍写
        # `Accept-Ranges: none`，那是待修订的文档，不是本脚本接受的契约；若实现仍
        # 返回 none/200，这里的失败就是需要 Sidecar 对齐的契约差异。
        self.assertEqual(_header(headers, "Accept-Ranges"), "bytes")

        self._phase("download head")
        status, headers, data = self.http.request("HEAD", old_ticket_url)
        self.assertEqual(status, 200)
        self.assertEqual(data, b"", "HEAD 不能带响应体")
        self.assertEqual(_header(headers, "Content-Length"), str(len(payload_bytes)))
        self.assertEqual(_header(headers, "Accept-Ranges"), "bytes")

        self._phase("range single")
        # 标准单区间 `bytes=4-7`：206 + 精确切片 + Content-Range 全长，是下载管理器
        # 断点续传的最小依赖，任何一个字段错都会拼出错误文件。
        status, headers, data = self.http.request(
            "GET", old_ticket_url, {"Range": "bytes=4-7"}
        )
        self.assertEqual(status, 206, "Range 请求必须返回 206")
        self.assertEqual(data, payload_bytes[4:8], "单区间切片字节不符")
        self.assertEqual(
            _header(headers, "Content-Range"),
            f"bytes 4-7/{len(payload_bytes)}",
        )
        self.assertEqual(_header(headers, "Content-Length"), "4")

        self._phase("range open ended")
        # 省略末端的 `bytes=N-`：浏览器恢复下载最常见的形态，必须读到文件末尾。
        status, headers, data = self.http.request(
            "GET", old_ticket_url, {"Range": f"bytes={len(payload_bytes) - 8}-"}
        )
        self.assertEqual(status, 206)
        self.assertEqual(data, payload_bytes[-8:], "开区间切片字节不符")
        self.assertEqual(
            _header(headers, "Content-Range"),
            f"bytes {len(payload_bytes) - 8}-{len(payload_bytes) - 1}/{len(payload_bytes)}",
        )

        self._phase("range suffix")
        # 后缀区间 `bytes=-4`：取最后 4 字节，同样必须按 206 语义返回。
        status, headers, data = self.http.request(
            "GET", old_ticket_url, {"Range": "bytes=-4"}
        )
        self.assertEqual(status, 206)
        self.assertEqual(data, payload_bytes[-4:], "后缀区间切片字节不符")
        self.assertEqual(
            _header(headers, "Content-Range"),
            f"bytes {len(payload_bytes) - 4}-{len(payload_bytes) - 1}/{len(payload_bytes)}",
        )

        self._phase("range unsatisfiable")
        # 起点等于文件大小：语义上没有任何字节可给，必须是 416 + 完整长度的
        # Content-Range，而不是 200 整份回退——整份回退会让续传端拼出重复数据。
        status, headers, _ = self.http.request(
            "GET", old_ticket_url, {"Range": f"bytes={len(payload_bytes)}-"}
        )
        self.assertEqual(status, 416, "超范围 Range 必须返回 416")
        self.assertEqual(
            _header(headers, "Content-Range"), f"bytes */{len(payload_bytes)}"
        )

        self._phase("range invalid")
        # 语法非法（单位未知、区间倒序）按 RFC 7233 忽略并整份返回 200：
        # 误判成 416 会让下载器放弃一次本可成功的下载。Rust 契约测试
        # `unusable_range_is_ignored_and_returns_full_body` 固定同一语义。
        status, headers, data = self.http.request(
            "GET", old_ticket_url, {"Range": "items=0-4"}
        )
        self.assertEqual(status, 200, "未知 Range 单位应忽略并整份返回")
        self.assertEqual(data, payload_bytes, "未知 Range 单位不能改变响应体")
        self.assertIsNone(_header(headers, "Content-Range"))
        status, headers, data = self.http.request(
            "GET", old_ticket_url, {"Range": "bytes=8-4"}
        )
        self.assertEqual(status, 200, "倒序 Range 应忽略并整份返回")
        self.assertEqual(data, payload_bytes, "倒序 Range 不能改变响应体")
        self.assertIsNone(_header(headers, "Content-Range"))

        self._phase("utf8 download")
        status, _, data = self.http.request(
            "POST",
            "/api/download-ticket",
            {
                "Authorization": f"Bearer {self.token}",
                "Origin": self.http.origin,
                "Content-Type": "application/json",
            },
            _json_bytes({"fileId": "root/报告 2026.txt"}),
        )
        ticket = self._json(status, data, "ticket utf8")
        status, _, data = self.http.request("GET", ticket["url"])
        self.assertEqual(status, 200)
        self.assertEqual(data, b"utf8-name\n")

        self._phase("upload origin rejection")
        status, _, _ = self.http.upload(
            self.token, "csrf.bin", b"x", origin="http://attacker.example"
        )
        self.assertEqual(status, 403)
        self.assertFalse((self.share / "csrf.bin").exists(), "被拒绝的上传不能落盘")
        status, _, _ = self.http.upload(self.token, "no-origin.bin", b"x", include_origin=False)
        self.assertEqual(status, 403)
        self.assertFalse((self.share / "no-origin.bin").exists())

        self._phase("host rejection")
        status, headers, _ = self.http.request(
            "GET", "/", {"Host": "attacker.example:1234"}
        )
        self.assertEqual(status, 400)
        self.assertEqual(_header(headers, "X-Frame-Options"), "DENY")

        self._phase("path traversal rejection")
        status, _, data = self.http.request(
            "GET",
            f"/api/list?dir={_percent('root/../etc')}",
            {"Authorization": f"Bearer {self.token}"},
        )
        payload = self._json(status, data, "traversal", expected=403)
        self.assertEqual(payload["error"]["code"], "path_rejected")

        self._phase("aborted upload cleanup")
        self._abort_upload("aborted.bin")
        if not self._wait_no_visible_staging(ABORT_CLEANUP_TIMEOUT):
            self.fail("客户端中断上传后仍留下最终文件或可见暂存项")
        # 中断请求必须让服务恢复可用。一次上传成功只能证伪「并发占位全部泄漏」：
        # 服务端不暴露占位计数，无法逐请求核算归还，这里不声称更强的结论。
        status, _, data = self.http.upload(self.token, "after-abort.bin", b"ok")
        uploaded = self._json(status, data, "upload after abort")
        self.assertEqual(uploaded["name"], "after-abort.bin")
        self.assertNotEqual(status, 429, "中断后服务不应停留在占位耗尽状态")

        self._phase("stop")
        result = self._expect_ok(
            self.rpc.request(
                "view.action", {"instance": "e2e", "action": "stop", "data": {}}, RPC_TIMEOUT
            ),
            "stop",
        )
        self.assertFalse(result["patch"]["running"])
        self.assertIsNone(result["patch"]["address"])
        self._wait_port_closed(port)
        self._assert_no_staging()

        self._phase("restart")
        result = self._expect_ok(
            self.rpc.request(
                "view.action",
                {
                    "instance": "e2e",
                    "action": "start",
                    "data": {
                        "root_path": str(self.share),
                        "selected_address": "127.0.0.1",
                    },
                },
                RPC_TIMEOUT,
            ),
            "restart",
        )
        _, new_port, new_secret = _parse_link(result["patch"]["link"])
        # 用布尔断言而不是 assertNotEqual：后者失败时会把新旧配对密钥原值
        # 打进 unittest 的诊断输出，这里只允许固定说明文本。
        self.assertTrue(
            new_secret != secret, "重启必须生成新的配对密钥（不回显原值）"
        )
        self.http = HttpApi(new_port)

        self._phase("old credentials invalidated")
        status, _, data = self.http.request(
            "GET", "/api/session", {"Authorization": f"Bearer {self.token}"}
        )
        payload = self._json(status, data, "old token", expected=401)
        self.assertEqual(payload["error"]["code"], "unauthorized")
        status, _, data = self.http.request("GET", old_ticket_url)
        payload = self._json(status, data, "old ticket", expected=410)
        self.assertEqual(payload["error"]["code"], "ticket_expired")
        status, _, data = self.http.pair(secret)
        payload = self._json(status, data, "old secret", expected=401)
        self.assertEqual(payload["error"]["code"], "invalid_secret")

        self._phase("pair after restart")
        status, _, data = self.http.pair(new_secret)
        payload = self._json(status, data, "pair after restart")
        self.token = payload["token"]
        status, _, data = self.http.request(
            "GET", "/api/list?dir=root", {"Authorization": f"Bearer {self.token}"}
        )
        payload = self._json(status, data, "list after restart")
        self.assertIn("payload.bin", {entry["name"] for entry in payload["entries"]})

        self._phase("stop again")
        self._expect_ok(
            self.rpc.request(
                "view.action", {"instance": "e2e", "action": "stop", "data": {}}, RPC_TIMEOUT
            ),
            "stop again",
        )
        self._wait_port_closed(new_port)

        self._phase("shutdown")
        self._expect_ok(self.rpc.request("shutdown", {}, EXIT_TIMEOUT), "shutdown")
        code = self.rpc.wait_exit(EXIT_TIMEOUT)
        self.assertEqual(code, 0, "shutdown 后必须正常退出")
        self.assertIsNotNone(self.rpc.proc.poll(), "进程未被回收")
        self._wait_port_closed(new_port, timeout=2)
        self._assert_no_staging()

    def _wait_no_visible_staging(self, timeout):
        """等待命名暂存与最终文件从共享目录消失；返回是否在窗口内清空。

        这同样只是「可见形态已消失」的下限证明：与 `_assert_no_staging` 一致，
        匿名暂存与任何未来新增的暂存材料不在此断言的观察能力之内。
        """
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            names = os.listdir(self.share)
            if "aborted.bin" not in names and not any(
                name.startswith(".klt-tmp-") for name in names
            ):
                return True
            time.sleep(0.1)
        return False

    def _abort_upload(self, name):
        """模拟浏览器取消：先同步到服务端已真正接收上传，再断开连接。

        直接断开可能在服务端创建暂存之前就结束，让「无残留」变成空验证。Windows
        （及 Unix 命名回退）上暂存是可见的 `.klt-tmp-*` 目录项，可以轮询等到它出现；
        Linux 主路径匿名暂存不可见，降级为一个有界静默窗口后断开，只能验证无最终
        文件、无可见残留，不声称清理了已观察到的暂存。并发占位方面，后续一次上传
        成功只能证伪「全部占位泄漏」，不证明逐请求归还，结论在测试正文里如实标注。
        """
        request = (
            f"POST /api/upload?dir=root&name={_percent(name)} HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{self.http.port}\r\n"
            f"Origin: {self.http.origin}\r\n"
            f"Authorization: Bearer {self.token}\r\n"
            "Content-Type: application/octet-stream\r\n"
            "Content-Length: 1048576\r\n"
            "Connection: close\r\n"
            "\r\n"
        ).encode("ascii")
        sock = socket.create_connection(("127.0.0.1", self.http.port), timeout=2)
        try:
            sock.sendall(request + b"x" * 4096)
        except OSError:
            pass
        if os.name == "nt":
            # Windows 暂存是命名文件：等到它真的出现在共享目录里，清理断言才有对象。
            deadline = time.monotonic() + ABORT_STAGING_APPEAR_TIMEOUT
            while time.monotonic() < deadline:
                if any(n.startswith(".klt-tmp-") for n in os.listdir(self.share)):
                    break
                time.sleep(0.05)
            else:
                self.fail("中断前未观察到服务端暂存，无法验证清理路径")
        else:
            # 匿名暂存没有目录项：退化为有界静默窗口，只保证请求已被服务端接收处理。
            time.sleep(ABORT_STAGING_APPEAR_TIMEOUT)
        sock.close()


def main(argv=None):
    """解析显式传入的二进制并运行测试；没有二进制时只做脚本自检并明确标注未联调。"""
    global ARGS
    # 诊断文本含中文：固定 UTF-8 输出，避免 Windows 控制台代码页差异把断言信息
    # 变成乱码或在遇到不可编码字符时抛出 UnicodeEncodeError 掩盖真实失败。
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass
    default_manifest = Path(__file__).resolve().parents[1] / "kerminal-plugin.json"
    parser = argparse.ArgumentParser(description="真实 Sidecar 黑盒 HTTP/JSON-RPC 回归")
    parser.add_argument("--sidecar", type=Path, help="显式指定要运行的 Sidecar 二进制")
    parser.add_argument(
        "--manifest",
        type=Path,
        default=default_manifest,
        help="插件清单（读取 id/version，默认仓库内 kerminal-plugin.json）",
    )
    parser.add_argument("--plugin-version", help="覆盖握手版本（二进制用 KERMINAL_PLUGIN_VERSION 构建时使用）")
    parser.add_argument("--timeout", type=float, default=180.0, help="整个联调流程的秒级上限")
    parser.add_argument("--verbose", action="store_true", help="输出阶段与子进程标准错误")
    ARGS = parser.parse_args(argv)
    if ARGS.sidecar is not None:
        ARGS.sidecar = ARGS.sidecar.resolve()

    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromTestCase(HelperSelfCheck))
    suite.addTests(loader.loadTestsFromTestCase(SidecarEndToEnd))
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if ARGS.sidecar is None:
        print(
            "NOTE: 未提供 --sidecar；真实 Sidecar 联调未运行，以上仅完成脚本自检。",
            file=sys.stderr,
        )
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
