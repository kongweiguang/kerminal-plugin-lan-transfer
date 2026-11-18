# 发布工作流结构门禁测试：完全本地离线，不调用 GitHub、网络、签名或公证。
# 测试对象是 .github/workflows/release.yml 内嵌脚本的真实逻辑：plan 的平台核对、
# 依赖锁定前置检查、publish 的期望资产集合、市场条目预生成与 macOS 签名/公证
# fail-closed 门禁。gh 用本机假命令按场景返回，仅模拟命令行契约，不代表真实 GitHub
# 行为；Cargo.lock 门禁用本机 pwsh 执行工作流中的真实片段，不构建、不联网。
# bash step 的报错标注（echo ::error::）写在标准输出，断言以 stdout 为准。
# @author kongweiguang

import json
import locale
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

WORKFLOW = (
    Path(__file__).resolve().parents[1]
    / ".github" / "workflows" / "release.yml"
)

FOUR_PLATFORMS = ["windows-x86_64", "linux-x86_64", "macos-x86_64", "macos-aarch64"]
PLUGIN_ID = "kongweiguang.lan-transfer"


def _step(job: str, name: str) -> dict:
    """取工作流中指定 job/step，避免各处重复解析 YAML。"""
    workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
    for step in workflow["jobs"][job]["steps"]:
        if step.get("name") == name:
            return step
    raise AssertionError(f"workflow 缺少 step：{job}/{name}")


def _step_run(job: str, name: str) -> str:
    """提取指定 step 的 run 脚本，保证测试驱动的是真实脚本内容。"""
    return _step(job, name)["run"]


def _step_python(job: str, name: str) -> str:
    """提取 bash step 内 `python3 - <<'PY'` heredoc 的 Python 源码，直接用解释器执行。"""
    script = _step_run(job, name)
    match = re.search(r"python3 - <<'PY'\n(.*?)\nPY\n", script, re.S)
    assert match, f"step {job}/{name} 中找不到 Python heredoc"
    return match.group(1)


def _sidecar_manifest(platforms: list[str], version: str = "0.1.0") -> dict:
    return {
        "schema": 1,
        "id": PLUGIN_ID,
        "version": version,
        "categories": ["network"],
        "runtime": {
            "kind": "sidecar",
            "main": {
                platform: "bin/kerminal-plugin-sidecar.exe"
                if platform.startswith("windows")
                else "bin/kerminal-plugin-sidecar"
                for platform in platforms
            },
        },
    }


def _none_manifest(version: str = "0.1.0") -> dict:
    """runtime.kind=none：无 Sidecar，发布集合只有 any 单包。"""
    return {
        "schema": 1,
        "id": PLUGIN_ID,
        "version": version,
        "categories": ["network"],
        "runtime": {"kind": "none"},
    }


def _base_env() -> dict:
    env = dict(os.environ)
    env["PYTHONIOENCODING"] = "utf-8"
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_SYSTEM"] = os.devnull
    # Windows 上 Git Bash 的 python3 命中 Microsoft Store 占位程序且无输出地失败；
    # 本地执行 bash step 时用 shim 把 python3 指到当前解释器，仅影响测试进程环境。
    # Git Bash 只认无扩展的 sh 脚本，.cmd 不参与其命令解析。
    shim = Path(os.environ["TEMP"]) / "kerminal-wf-shim"
    shim.mkdir(parents=True, exist_ok=True)
    exe = shim / "python3"
    if not exe.exists() or os.environ.get("KERMINAL_WF_SHIM_REFRESH") == "1":
        exe.write_text(
            "#!/bin/sh\nexec \"%s\" \"$@\"\n" % Path(sys.executable).as_posix(),
            encoding="utf-8",
        )
        if os.name != "nt":
            exe.chmod(0o755)
    env["PATH"] = str(shim) + os.pathsep + env["PATH"]
    return env


@pytest.fixture(scope="module")
def plan_script() -> str:
    return _step_python("plan", "Validate release input")


def _bash_command() -> list[str]:
    """定位 Git Bash：Windows 上 PATH 里按名解析的 bash 会命中 System32 的 WSL bash，
    它剥离 Windows 环境变量，无法模拟 GitHub runner 的 bash step 语义。"""
    if os.name != "nt":
        return ["bash"]
    bash = shutil.which("bash")
    if bash and "system32" not in bash.lower():
        return [bash]
    for candidate in (r"C:\Program Files\Git\bin\bash.exe",
                      r"C:\Program Files\Git\usr\bin\bash.exe"):
        if Path(candidate).is_file():
            return [candidate]
    raise AssertionError("未找到 Git Bash，无法执行 bash step 测试")


@pytest.fixture(scope="module")
def publish_script() -> str:
    return _step_python("publish", "Verify complete candidate set")


@pytest.fixture(scope="module")
def lock_precheck_script() -> str:
    return _step_run("build", "Verify committed Cargo.lock before building")


@pytest.fixture(scope="module")
def signing_script() -> str:
    return _step_run("build", "Sign and notarize macOS Sidecar when configured")


# ---------------------------------------------------------------------------
# plan / publish 校验脚本
# ---------------------------------------------------------------------------

def _run_plan(
    script: str,
    tmp_path: Path,
    manifest: dict,
    ref_name: str,
) -> subprocess.CompletedProcess:
    """在受控目录执行 plan 脚本；调用方断言退出码并按需读取 GITHUB_OUTPUT。"""
    (tmp_path / "kerminal-plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
    env = _base_env()
    env.update({
        "GITHUB_REF_NAME": ref_name,
        "SDK_REPOSITORY": "kongweiguang/Kerminal",
        "SDK_REF": "a" * 40,
        "GITHUB_OUTPUT": str(tmp_path / "github-output.txt"),
    })
    return subprocess.run(
        [sys.executable, "-c", script],
        cwd=tmp_path, env=env, capture_output=True,
        text=True, encoding="utf-8", errors="replace", timeout=60,
    )


def _plan_outputs(tmp_path: Path) -> dict:
    outputs = {}
    for line in (tmp_path / "github-output.txt").read_text(encoding="utf-8").splitlines():
        key, _, value = line.partition("=")
        outputs[key] = value
    return outputs


def _run_publish(
    script: str,
    tmp_path: Path,
    manifest: dict,
    dist_names: list[str],
    platforms: list[str],
    ref_name: str = "v0.1.0",
) -> subprocess.CompletedProcess:
    """在受控目录执行 publish 校验脚本；调用方按需断言退出码与输出。"""
    (tmp_path / "kerminal-plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
    dist = tmp_path / "dist"
    dist.mkdir(exist_ok=True)
    for name in dist_names:
        (dist / name).write_bytes(b"kplugin:" + name.encode())
    env = _base_env()
    env.update({
        "GITHUB_REF_NAME": ref_name,
        "EXPECTED_PLATFORMS": json.dumps(platforms),
        "RUNNER_TEMP": str(tmp_path / "runner-temp"),
        "GITHUB_REPOSITORY": "kongweiguang/kerminal-plugin-lan-transfer",
    })
    (tmp_path / "runner-temp").mkdir(exist_ok=True)
    return subprocess.run(
        [sys.executable, "-c", script],
        cwd=tmp_path, env=env, capture_output=True,
        text=True, encoding="utf-8", errors="replace", timeout=60,
    )


def test_yaml_available():
    """PyYAML 缺失时显式失败，不用 grep 冒充结构验证。"""
    assert yaml is not None, "需要 PyYAML 才能对工作流做结构验证"


def test_plan_accepts_four_platform_manifest_and_outputs_matrix(plan_script, tmp_path):
    """四平台清单通过，输出矩阵与平台列表完全一致。"""
    completed = _run_plan(plan_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS), "v0.1.0")
    assert completed.returncode == 0, completed.stderr
    outputs = _plan_outputs(tmp_path)
    assert json.loads(outputs["platforms"]) == FOUR_PLATFORMS
    assert [entry["platform"] for entry in json.loads(outputs["matrix"])["include"]] == FOUR_PLATFORMS
    assert outputs["sdk_ref"] == "a" * 40


@pytest.mark.parametrize("declared", [
    ["windows-x86_64", "linux-x86_64", "macos-x86_64"],
    ["windows-x86_64", "linux-x86_64", "macos-x86_64", "macos-aarch64", "windows-aarch64"],
    ["windows-x86_64", "linux-x86_64", "macos-x86_64", "macos-aarch64", "any"],
])
def test_plan_fails_on_platform_mismatch(plan_script, tmp_path, declared):
    """清单漏列、新增或混入 any 平台都必须失败，不允许静默漏包。"""
    completed = _run_plan(plan_script, tmp_path, _sidecar_manifest(declared), "v0.1.0")
    assert completed.returncode != 0
    assert "完全一致" in completed.stderr


def test_plan_fails_on_tag_mismatch(plan_script, tmp_path):
    """tag 与清单版本不一致时失败。"""
    completed = _run_plan(
        plan_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS, version="0.2.0"), "v0.1.0")
    assert completed.returncode != 0
    assert "tag 必须严格等于清单版本" in completed.stderr


def test_plan_accepts_prerelease_tag(plan_script, tmp_path):
    """预发布版本（0.1.0-beta.1）与同名 v 前缀 tag 是合法输入，矩阵仍为四平台。"""
    completed = _run_plan(
        plan_script, tmp_path,
        _sidecar_manifest(FOUR_PLATFORMS, version="0.1.0-beta.1"),
        "v0.1.0-beta.1")
    assert completed.returncode == 0, completed.stderr
    assert json.loads(_plan_outputs(tmp_path)["platforms"]) == FOUR_PLATFORMS


def test_plan_accepts_none_runtime_with_any_platform(plan_script, tmp_path):
    """runtime.kind=none 的清单输出单平台 any 矩阵，不要求 Sidecar 平台键。"""
    completed = _run_plan(plan_script, tmp_path, _none_manifest(), "v0.1.0")
    assert completed.returncode == 0, completed.stderr
    outputs = _plan_outputs(tmp_path)
    assert json.loads(outputs["platforms"]) == ["any"]
    matrix = json.loads(outputs["matrix"])["include"]
    assert len(matrix) == 1 and matrix[0]["platform"] == "any" and matrix[0]["target"] == ""


def _four_names(version: str = "0.1.0") -> list[str]:
    return [f"{PLUGIN_ID}-{version}-{platform}.kplugin" for platform in FOUR_PLATFORMS]


def test_publish_accepts_full_four_platform_set(publish_script, tmp_path):
    """四平台正例：退出码为 0，且生成的市场条目包含四个平台包对应的版本与 tag。"""
    completed = _run_publish(
        publish_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS), _four_names(), FOUR_PLATFORMS)
    assert completed.returncode == 0, completed.stderr
    entry = (tmp_path / "runner-temp" / "marketplace-entry.toml").read_text(encoding="utf-8")
    assert f"plugins/{PLUGIN_ID}.toml" in entry
    assert 'version = "0.1.0"' in entry
    assert 'tag = "v0.1.0"' in entry
    # 资产集合断言由校验脚本自身完成；这里确认四个包名确实都在 dist 中。
    for name in _four_names():
        assert (tmp_path / "dist" / name).is_file()


def test_publish_accepts_none_runtime_single_any_asset(publish_script, tmp_path):
    """none 形态：仅接受单个 -any.kplugin，版本与 tag 一致时通过。"""
    completed = _run_publish(
        publish_script, tmp_path, _none_manifest(),
        [f"{PLUGIN_ID}-0.1.0-any.kplugin"], ["any"])
    assert completed.returncode == 0, completed.stderr


def test_publish_accepts_prerelease_version_and_tag(publish_script, tmp_path):
    """预发布版本：资产名、清单版本与 v 前缀 tag 三者一致时通过。"""
    version = "0.1.0-beta.1"
    completed = _run_publish(
        publish_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS, version=version),
        _four_names(version), FOUR_PLATFORMS, ref_name="v" + version)
    assert completed.returncode == 0, completed.stderr
    entry = (tmp_path / "runner-temp" / "marketplace-entry.toml").read_text(encoding="utf-8")
    assert f'tag = "v{version}"' in entry


def test_publish_rejects_missing_platform_asset(publish_script, tmp_path):
    """缺一个平台的包即失败，不进入发布。"""
    completed = _run_publish(
        publish_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS), _four_names()[:3], FOUR_PLATFORMS)
    assert completed.returncode != 0
    assert "不完整" in completed.stderr


def test_publish_rejects_unexpected_platform_asset(publish_script, tmp_path):
    """dist 中出现期望之外的平台包同样失败。"""
    names = _four_names() + [f"{PLUGIN_ID}-0.1.0-windows-aarch64.kplugin"]
    completed = _run_publish(
        publish_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS), names, FOUR_PLATFORMS)
    assert completed.returncode != 0
    assert "不完整" in completed.stderr


def test_publish_rejects_platform_list_drift(publish_script, tmp_path):
    """plan 平台列表与 runtime.kind 期望不符时失败，防止两处清单漂移。"""
    completed = _run_publish(
        publish_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS),
        [f"{PLUGIN_ID}-0.1.0-any.kplugin"], ["any"])
    assert completed.returncode != 0
    assert "不一致" in completed.stderr


def test_publish_rejects_tag_mismatch(publish_script, tmp_path):
    """publish 侧 tag 与清单版本不一致同样失败。"""
    completed = _run_publish(
        publish_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS), _four_names(),
        FOUR_PLATFORMS, ref_name="v9.9.9")
    assert completed.returncode != 0
    assert "tag 与清单版本" in completed.stderr


def test_publish_rejects_invalid_categories_before_release_writes(publish_script, tmp_path):
    """categories 非字符串数组时校验在发布写操作前失败，不产生市场条目文件。"""
    manifest = _sidecar_manifest(FOUR_PLATFORMS)
    manifest["categories"] = ["network", 3]
    completed = _run_publish(publish_script, tmp_path, manifest, _four_names(), FOUR_PLATFORMS)
    assert completed.returncode != 0
    assert "categories" in completed.stderr
    assert not (tmp_path / "runner-temp" / "marketplace-entry.toml").exists()


def test_publish_generates_marketplace_entry_before_release(publish_script, tmp_path):
    """清单与资产全部通过时，市场条目文件在发布写操作之前已经生成。"""
    completed = _run_publish(
        publish_script, tmp_path, _sidecar_manifest(FOUR_PLATFORMS), _four_names(), FOUR_PLATFORMS)
    assert completed.returncode == 0, completed.stderr
    entry = (tmp_path / "runner-temp" / "marketplace-entry.toml").read_text(encoding="utf-8")
    assert f"plugins/{PLUGIN_ID}.toml" in entry
    assert 'version = "0.1.0"' in entry
    assert 'tag = "v0.1.0"' in entry


# ---------------------------------------------------------------------------
# Cargo.lock 门禁：前置 git 检查用 Git Bash，构建后哈希用本机真实 pwsh 执行
# 工作流中的原始片段（不构建、不联网、不触碰仓库真实 lock）。
# ---------------------------------------------------------------------------

def _init_git_with_lock(tmp_path: Path, lock_content: str, tracked: bool) -> Path:
    """初始化真实 git 仓库并按 tracked 标志准备 Cargo.lock 状态。"""
    subprocess.run(["git", "init", "-q"], cwd=tmp_path, check=True,
                   env=_base_env(), capture_output=True)
    lock = tmp_path / "Cargo.lock"
    lock.write_text(lock_content, encoding="utf-8")
    if tracked:
        subprocess.run(["git", "-C", str(tmp_path), "add", "Cargo.lock"], check=True,
                       env=_base_env(), capture_output=True)
        subprocess.run(
            ["git", "-C", str(tmp_path), "-c", "user.email=t@t", "-c", "user.name=t",
             "commit", "-qm", "lock"],
            check=True, env=_base_env(), capture_output=True)
    return lock


def _run_lock_precheck(script: str, cwd: Path, extra_path: Path | None = None) -> subprocess.CompletedProcess:
    env = _base_env()
    env["RUNNER_TEMP"] = str(cwd)
    if extra_path is not None:
        env["PATH"] = str(extra_path) + os.pathsep + env["PATH"]
    return subprocess.run(
        [*_bash_command(), "-c", script],
        cwd=cwd, env=env, capture_output=True,
        text=True, encoding="utf-8", errors="replace", timeout=120,
    )


def _write_stub(room: Path, name: str, body: str) -> Path:
    """写一个记录调用并以固定状态码退出的本地 stub，用于替代 cargo 等外部命令。"""
    bin_dir = room / "stub-bin"
    bin_dir.mkdir(exist_ok=True)
    stub = bin_dir / name
    stub.write_text("#!/bin/sh\n" + body, encoding="utf-8")
    if os.name != "nt":
        stub.chmod(0o755)
    return bin_dir


def test_lock_precheck_rejects_missing_lock(lock_precheck_script, tmp_path):
    """Cargo.lock 不存在时前置检查必须失败。"""
    completed = _run_lock_precheck(lock_precheck_script, tmp_path)
    assert completed.returncode != 0
    assert "未提交 Cargo.lock" in completed.stdout


def test_lock_precheck_rejects_untracked_lock(lock_precheck_script, tmp_path):
    """Cargo.lock 存在但未被 git 跟踪时必须失败；用真实 git 仓库验证。"""
    _init_git_with_lock(tmp_path, "# lock\n", tracked=False)
    completed = _run_lock_precheck(lock_precheck_script, tmp_path)
    assert completed.returncode != 0
    assert "未被 git 跟踪" in completed.stdout


def test_lock_precheck_rejects_dirty_lock(lock_precheck_script, tmp_path):
    """Cargo.lock 已跟踪但工作树有未提交变化时必须失败。"""
    lock = _init_git_with_lock(tmp_path, "# lock\n", tracked=True)
    lock.write_text("# lock modified\n", encoding="utf-8")
    completed = _run_lock_precheck(lock_precheck_script, tmp_path)
    assert completed.returncode != 0
    assert "未提交的工作树或索引变化" in completed.stdout


def test_lock_precheck_rejects_staged_dirty_lock(lock_precheck_script, tmp_path):
    """Cargo.lock 已跟踪但索引里有未提交的暂存变化时也必须失败。"""
    lock = _init_git_with_lock(tmp_path, "# lock\n", tracked=True)
    lock.write_text("# staged change\n", encoding="utf-8")
    subprocess.run(["git", "-C", str(tmp_path), "add", "Cargo.lock"], check=True,
                   env=_base_env(), capture_output=True)
    completed = _run_lock_precheck(lock_precheck_script, tmp_path)
    assert completed.returncode != 0
    assert "未提交的工作树或索引变化" in completed.stdout


def test_lock_precheck_passes_with_stub_cargo_and_snapshots_lock(lock_precheck_script, tmp_path):
    """干净锁文件通过全部 git 门禁；用 stub cargo 使 fetch 成功，确认快照已写出。

    该用例验证的是门禁顺序与快照动作，不是真实依赖解析（真实 fetch 需网络）。
    """
    lock = _init_git_with_lock(tmp_path, "# lock\n", tracked=True)
    bin_dir = _write_stub(tmp_path, "cargo", 'echo "stub cargo $*" >> "$STUB_LOG"\nexit 0\n')
    env = _base_env()
    env.update({"RUNNER_TEMP": str(tmp_path), "STUB_LOG": str(tmp_path / "stub.log")})
    env["PATH"] = str(bin_dir) + os.pathsep + env["PATH"]
    completed = subprocess.run(
        [*_bash_command(), "-c", lock_precheck_script],
        cwd=tmp_path, env=env, capture_output=True,
        text=True, encoding="utf-8", errors="replace", timeout=120,
    )
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert "fetch --locked" in (tmp_path / "stub.log").read_text(encoding="utf-8")
    snapshot = tmp_path / "Cargo.lock.checked-out"
    assert snapshot.is_file() and snapshot.read_text(encoding="utf-8") == lock.read_text(encoding="utf-8")


def _pwsh() -> str:
    """定位真实 pwsh；缺失时由调用方显式 skip，不用 grep 冒充 PowerShell 验证。"""
    exe = shutil.which("pwsh") or shutil.which("pwsh.exe")
    if not exe:
        pytest.skip("本机没有 pwsh，无法执行构建后 Cargo.lock 哈希门禁片段")
    return exe


def _post_build_hash_gate_script() -> str:
    """从「Build and validate plugin」step 抽取构建后哈希门禁片段原文。"""
    script = _step_run("build", "Build and validate plugin")
    marker = "# 构建后逐字节比对 Cargo.lock"
    index = script.find(marker)
    assert index >= 0, "构建 step 中找不到 Cargo.lock 哈希门禁片段"
    return script[index:]


def _decode_console(data: bytes) -> str:
    """pwsh 在 Windows 控制台按本机代码页（本机为 cp936）输出，Linux runner 为 UTF-8；
    先按 UTF-8 严格解码，失败再退回本机首选编码，避免中文断言因编码误判。"""
    for encoding in ("utf-8", locale.getpreferredencoding(False) or "cp936", "gbk"):
        try:
            return data.decode(encoding)
        except (UnicodeDecodeError, LookupError):
            continue
    return data.decode("utf-8", "replace")


def _run_pwsh_gate(room: Path) -> subprocess.CompletedProcess:
    gate = room / "gate.ps1"
    gate.write_text(_post_build_hash_gate_script(), encoding="utf-8")
    env = _base_env()
    env["RUNNER_TEMP"] = str(room)
    completed = subprocess.run(
        [_pwsh(), "-NoProfile", "-NonInteractive", "-File", str(gate)],
        cwd=room, env=env, capture_output=True, timeout=120,
    )
    # 统一转成文本，调用方无需关心控制台编码。
    completed.stdout = _decode_console(completed.stdout)
    completed.stderr = _decode_console(completed.stderr)
    return completed


def test_post_build_lock_hash_gate_passes_when_unchanged(tmp_path):
    """构建前后锁文件一致：哈希门禁以 0 退出。"""
    (tmp_path / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
    (tmp_path / "Cargo.lock.checked-out").write_text("# lock\n", encoding="utf-8")
    completed = _run_pwsh_gate(tmp_path)
    assert completed.returncode == 0, completed.stdout + completed.stderr


def test_post_build_lock_hash_gate_fails_when_modified(tmp_path):
    """构建期间锁文件被改写：哈希门禁非零退出并给出明确原因。"""
    (tmp_path / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
    (tmp_path / "Cargo.lock.checked-out").write_text("# lock modified\n", encoding="utf-8")
    completed = _run_pwsh_gate(tmp_path)
    assert completed.returncode != 0
    combined = completed.stdout + completed.stderr
    assert "Cargo.lock 发生变化" in combined or "Cargo.lock" in combined


def test_post_build_lock_hash_gate_fails_when_snapshot_missing(tmp_path):
    """构建前快照缺失：门禁不能静默放行，必须非零退出。"""
    (tmp_path / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
    completed = _run_pwsh_gate(tmp_path)
    assert completed.returncode != 0


# ---------------------------------------------------------------------------
# macOS 签名/公证门禁
# ---------------------------------------------------------------------------

def _run_signing(script: str, room: Path, secrets: dict[str, str]) -> subprocess.CompletedProcess:
    """在受控目录执行签名 step：按构建清单声明的路径预置假二进制，让脚本推进到 Secret 门禁。"""
    manifest = _sidecar_manifest(["macos-aarch64"])
    build_dir = room / ".kerminal" / "build"
    build_dir.mkdir(parents=True, exist_ok=True)
    (build_dir / "kerminal-plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
    # 二进制必须位于清单 runtime.main 声明的相对路径，否则脚本在 test -f 处静默失败，
    # 门禁逻辑根本不会执行。
    declared = manifest["runtime"]["main"]["macos-aarch64"]
    binary = build_dir.joinpath(*declared.split("/"))
    binary.parent.mkdir(parents=True, exist_ok=True)
    binary.write_bytes(b"MZ")
    env = _base_env()
    env.update({"PLUGIN_PLATFORM": "macos-aarch64", "RUNNER_TEMP": str(room)})
    for name, value in secrets.items():
        env[name] = value
    return subprocess.run(
        [*_bash_command(), "-c", script],
        cwd=room, env=env, capture_output=True,
        text=True, encoding="utf-8", errors="replace", timeout=60,
    )


def test_signing_gate_fail_closed_when_all_missing(signing_script, tmp_path):
    """任何所需 Secret 缺失时签名 step 必须以非零码退出，不允许 warning 后继续。"""
    completed = _run_signing(signing_script, tmp_path, {
        "APPLE_CERTIFICATE_BASE64": "",
        "APPLE_CERTIFICATE_PASSWORD": "",
        "APPLE_SIGNING_IDENTITY": "",
        "APPLE_ID": "",
        "APPLE_TEAM_ID": "",
        "APPLE_APP_SPECIFIC_PASSWORD": "",
    })
    assert completed.returncode != 0, completed.stdout
    assert "缺少 Secret" in completed.stdout
    assert "::warning::" not in completed.stdout


def test_signing_gate_fail_closed_when_notarization_secrets_missing(signing_script, tmp_path):
    """签名 Secret 齐全而公证 Secret 缺失时同样 fail closed（旧工作流在此放行）。"""
    completed = _run_signing(signing_script, tmp_path, {
        "APPLE_CERTIFICATE_BASE64": "AAAA",
        "APPLE_CERTIFICATE_PASSWORD": "pw",
        "APPLE_SIGNING_IDENTITY": "Developer ID Application",
        "APPLE_ID": "",
        "APPLE_TEAM_ID": "",
        "APPLE_APP_SPECIFIC_PASSWORD": "",
    })
    assert completed.returncode != 0, completed.stdout
    assert "缺少 Secret" in completed.stdout
    assert "APPLE_ID" in completed.stdout
    assert "APPLE_APP_SPECIFIC_PASSWORD" in completed.stdout
    # 到达门禁即退出：不能触碰钥匙串或签名流程，目录里不应有签名痕迹。
    assert not list((tmp_path / ".kerminal" / "build").glob("*.p12"))


def test_signing_gate_reports_without_leaking_secret_values(signing_script, tmp_path):
    """报错只列 Secret 名称，任何 Secret 值都不得出现在输出中。"""
    secret = "top-secret-value-do-not-leak"
    completed = _run_signing(signing_script, tmp_path, {
        "APPLE_CERTIFICATE_BASE64": "AAAA",
        "APPLE_CERTIFICATE_PASSWORD": "pw",
        "APPLE_SIGNING_IDENTITY": "Developer ID Application",
        "APPLE_ID": "",
        "APPLE_TEAM_ID": secret,
        "APPLE_APP_SPECIFIC_PASSWORD": "",
    })
    assert completed.returncode != 0
    combined = completed.stdout + completed.stderr
    assert "缺少 Secret" in combined
    assert secret not in combined


def test_workflow_declares_gate_comments():
    """工作流头注释声明签名/公证门禁与锁定依赖门禁的存在与原因。"""
    text = WORKFLOW.read_text(encoding="utf-8")
    assert "必须完成" in text and "签名与公证" in text
    assert "--locked" in text
    assert "@author kongweiguang" in text
    # 注释必须与实际触发范围一致：所有 v*（含预发布）都走同一门禁。
    assert "任意 v* tag" in text


def test_install_cli_checks_full_sdk_tree():
    """CLI 安装 step 显式核验 sdk/cli、sdk/protocol 与 sdk/rust 三个必需目录。"""
    script = _step_run("build", "Install CLI from the selected SDK commit")
    for required in ("sdk/cli/Cargo.toml", "sdk/protocol/Cargo.toml", "sdk/rust/Cargo.toml"):
        assert required in script
    # 核验必须发生在 cargo install 之前。
    assert script.find("sdk/protocol/Cargo.toml") < script.find("cargo install")


def test_release_step_requires_generated_marketplace_entry():
    """市场条目在任何 gh release 写操作之前的校验 step 生成；展示 step 只消费不校验。"""
    verify = _step_run("publish", "Verify complete candidate set")
    release = _step_run("publish", "Create draft and upload verified assets")
    show = _step_run("publish", "Show marketplace entry")
    assert "categories" in verify
    assert verify.find("categories") < verify.find("marketplace-entry.toml")
    assert "marketplace-entry" not in release
    assert "marketplace-entry.toml" in show
    assert "kerminal-plugin.json" not in show


# ---------------------------------------------------------------------------
# 发布 step：本机假 gh 按场景返回，仅模拟命令行契约，不代表真实 GitHub 行为。
# 状态保存在房间内的 server/ 目录，模拟远端已有资产；所有调用写入 journal。
# ---------------------------------------------------------------------------

MOCK_GH_PY = r'''
import json, os, shutil, sys
from pathlib import Path

state = Path(os.environ["MOCK_STATE"])
server = state / "server"
server.mkdir(parents=True, exist_ok=True)
journal = Path(os.environ["GH_JOURNAL"])
with journal.open("a", encoding="utf-8") as out:
    out.write("gh " + " ".join(sys.argv[1:]) + "\n")

scenario = os.environ["MOCK_SCENARIO"]
args = sys.argv[1:]
dist = Path("dist")


def flag_value(name):
    return args[args.index(name) + 1] if name in args else ""


def assets():
    return sorted(p.name for p in server.glob("*.kplugin"))


def is_draft():
    # 场景里除 published_* 外都以 draft 存在。
    return not scenario.startswith("published_")


def view():
    views_file = state / "views"
    count = int(views_file.read_text()) if views_file.exists() else 0
    count += 1
    views_file.write_text(str(count))
    if scenario == "fresh" and count == 1:
        # 首次查看代表 Release 尚不存在。
        return 1
    if scenario in ("draft_conflict",):
        payload = {"isDraft": True, "assets": [{"name": n} for n in assets()]}
    else:
        payload = {"isDraft": is_draft(), "assets": [{"name": n} for n in assets()]}
    sys.stdout.write(json.dumps(payload))
    return 0


def download():
    pattern = flag_value("--pattern")
    target = Path(flag_value("--dir"))
    target.mkdir(parents=True, exist_ok=True)
    source = server / pattern
    if not source.is_file():
        sys.stderr.write("mock gh: 远端缺少资产 " + pattern + "\n")
        return 1
    if scenario == "readback_fail" and (state / "uploaded").exists() and pattern == assets()[0]:
        # 上传后回读阶段：返回损坏内容，模拟传输损坏。
        (target / pattern).write_bytes(b"corrupted")
        return 0
    shutil.copyfile(source, target / pattern)
    return 0


def upload():
    # gh release upload TAG FILE...；args[2] 是 tag，实际文件从 args[3] 开始。
    for arg in args[3:]:
        if arg.startswith("-"):
            continue
        shutil.copyfile(arg, server / Path(arg).name)
    return 0


def main():
    if len(args) < 2 or args[0] != "release":
        return 0
    command = args[1]
    if command == "view":
        return view()
    if command == "create":
        (state / "created").write_text(" ".join(args))
        return 0
    if command == "download":
        return download()
    if command == "upload":
        (state / "uploaded").write_text("yes")
        return upload()
    if command == "edit":
        if "--draft=false" in args:
            (state / "published").write_text("yes")
        return 0
    return 0


sys.exit(main())
'''


def _write_mock_gh(room: Path) -> None:
    """写本机假 gh：sh 包装只负责转发到 python 处理器，保证状态可跨调用保留。"""
    bin_dir = room / "bin"
    bin_dir.mkdir(exist_ok=True)
    handler = room / "mock_gh.py"
    handler.write_text(MOCK_GH_PY, encoding="utf-8")
    gh = bin_dir / "gh"
    gh.write_text(
        "#!/bin/sh\n"
        'exec "%s" "%s" "$@"\n' % (Path(sys.executable).as_posix(), handler.as_posix()),
        encoding="utf-8",
    )
    if os.name != "nt":
        gh.chmod(0o755)


def _prepare_release_room(
    room: Path,
    manifest: dict,
    dist_names: list[str],
    remote_assets: dict[str, bytes] | None = None,
) -> None:
    """准备发布 step 的受控环境：清单、dist 资产、市场条目、假 gh 与远端已有资产。"""
    (room / "kerminal-plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
    dist = room / "dist"
    dist.mkdir(exist_ok=True)
    for name in dist_names:
        (dist / name).write_bytes(b"kplugin:" + name.encode())
    runner_temp = room / "runner-temp"
    runner_temp.mkdir(exist_ok=True)
    (runner_temp / "marketplace-entry.toml").write_text("### entry\n", encoding="utf-8")
    state = room / "state"
    server = state / "server"
    server.mkdir(parents=True, exist_ok=True)
    for name, content in (remote_assets or {}).items():
        (server / name).write_bytes(content)
    _write_mock_gh(room)


def _run_release_step(
    room: Path,
    scenario: str,
    manifest: dict,
    dist_names: list[str],
    remote_assets: dict[str, bytes] | None = None,
    ref_name: str = "v0.1.0",
) -> tuple[subprocess.CompletedProcess, list[str], Path]:
    """以假 gh 执行发布 step，返回（结果, gh 调用日志, 状态目录）。"""
    _prepare_release_room(room, manifest, dist_names, remote_assets)
    journal = room / "gh-journal.txt"
    env = _base_env()
    env.update({
        "PATH": str(room / "bin") + os.pathsep + env["PATH"],
        "GH_TOKEN": "fake-token",
        "RELEASE_TAG": ref_name,
        "GH_JOURNAL": str(journal),
        "MOCK_SCENARIO": scenario,
        "MOCK_STATE": str(room / "state"),
        "RUNNER_TEMP": str(room / "runner-temp"),
    })
    completed = subprocess.run(
        [*_bash_command(), "-c", _step_run("publish", "Create draft and upload verified assets")],
        cwd=room, env=env, capture_output=True,
        text=True, encoding="utf-8", errors="replace", timeout=120,
    )
    calls = journal.read_text(encoding="utf-8").splitlines() if journal.exists() else []
    return completed, calls, room / "state"


def _uploads(calls: list[str]) -> list[str]:
    """从调用日志提取上传的资产文件名；gh release upload 的路径带 dist/ 前缀。"""
    return [Path(call.rsplit(" ", 1)[-1]).name
            for call in calls if call.startswith("gh release upload ")]


def test_release_fresh_draft_uploads_all_and_publishes(tmp_path):
    """无既有 Release：创建 draft、上传全部四包、回读一致后取消 draft。"""
    completed, calls, state = _run_release_step(
        tmp_path, "fresh", _sidecar_manifest(FOUR_PLATFORMS), _four_names())
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert any(call.startswith("gh release create ") for call in calls)
    assert len(_uploads(calls)) == len(FOUR_PLATFORMS)
    assert (state / "published").is_file()


def test_release_draft_with_identical_assets_skips_reupload(tmp_path):
    """已有 draft 且远端两个资产逐字节一致：只上传缺失的两个，再取消 draft。"""
    names = _four_names()
    remote = {names[0]: b"kplugin:" + names[0].encode(), names[1]: b"kplugin:" + names[1].encode()}
    completed, calls, state = _run_release_step(
        tmp_path, "draft_partial_match", _sidecar_manifest(FOUR_PLATFORMS), names, remote)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert sorted(_uploads(calls)) == sorted(names[2:])
    # 一致的两个资产必须被下载并逐字节比较，而不是被直接跳过验证。
    downloads = [call for call in calls if call.startswith("gh release download ")]
    assert sum(1 for call in downloads if names[0] in call) >= 1
    assert sum(1 for call in downloads if names[1] in call) >= 1
    assert (state / "published").is_file()


def test_release_draft_rejects_conflicting_existing_asset(tmp_path):
    """已有 draft 的同名资产内容不同：拒绝覆盖、非零退出，且绝不取消 draft。"""
    names = _four_names()
    remote = {names[0]: b"different-bytes"}
    completed, calls, state = _run_release_step(
        tmp_path, "draft_conflict", _sidecar_manifest(FOUR_PLATFORMS), names, remote)
    assert completed.returncode != 0
    assert "拒绝覆盖" in completed.stderr
    assert not (state / "published").exists()


def test_release_published_with_all_identical_assets_succeeds_without_writes(tmp_path):
    """已公开 Release 且四包全部一致：成功退出，不创建、不上传、不编辑。"""
    names = _four_names()
    remote = {name: b"kplugin:" + name.encode() for name in names}
    completed, calls, _ = _run_release_step(
        tmp_path, "published_all_match", _sidecar_manifest(FOUR_PLATFORMS), names, remote)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert not any(call.startswith("gh release create ") for call in calls)
    assert not _uploads(calls)
    assert not any("--draft=false" in call for call in calls)


def test_release_published_missing_asset_is_rejected(tmp_path):
    """已公开 Release 缺少本次资产：无法补齐，必须失败并提示人工处理。"""
    names = _four_names()
    remote = {name: b"kplugin:" + name.encode() for name in names[:3]}
    completed, calls, _ = _run_release_step(
        tmp_path, "published_missing", _sidecar_manifest(FOUR_PLATFORMS), names, remote)
    assert completed.returncode != 0
    assert "已有公开 Release 缺少资产" in completed.stderr
    assert not _uploads(calls)


def test_release_published_with_foreign_asset_is_rejected(tmp_path):
    """已公开 Release 含不属于本次发布的插件包：直接失败，不覆盖也不删除。"""
    names = _four_names()
    remote = {name: b"kplugin:" + name.encode() for name in names}
    remote[f"{PLUGIN_ID}-9.9.9-linux-x86_64.kplugin"] = b"foreign"
    completed, calls, _ = _run_release_step(
        tmp_path, "published_extra", _sidecar_manifest(FOUR_PLATFORMS), names, remote)
    assert completed.returncode != 0
    assert "不属于本次发布" in completed.stderr
    assert not _uploads(calls)


def test_release_fails_when_readback_download_fails(tmp_path):
    """上传后回读失败：非零退出且不取消 draft，半成品不会被当成发布完成。"""
    completed, calls, state = _run_release_step(
        tmp_path, "readback_fail", _sidecar_manifest(FOUR_PLATFORMS), _four_names())
    assert completed.returncode != 0
    assert not (state / "published").exists()


def test_release_none_runtime_publishes_single_any_asset(tmp_path):
    """none 形态：单 any 包走同一发布流程，创建 draft 后上传并取消 draft。"""
    name = f"{PLUGIN_ID}-0.1.0-any.kplugin"
    completed, calls, state = _run_release_step(
        tmp_path, "fresh", _none_manifest(), [name])
    assert completed.returncode == 0, completed.stdout + completed.stderr
    assert _uploads(calls) == [name]
    assert (state / "published").is_file()


def test_release_prerelease_tag_passes_prerelease_flag(tmp_path):
    """预发布版本：创建 Release 时必须带 --prerelease，其余流程与正式版一致。"""
    version = "0.1.0-beta.1"
    names = _four_names(version)
    completed, calls, _ = _run_release_step(
        tmp_path, "fresh", _sidecar_manifest(FOUR_PLATFORMS, version=version), names,
        ref_name="v" + version)
    assert completed.returncode == 0, completed.stdout + completed.stderr
    creates = [call for call in calls if call.startswith("gh release create ")]
    assert len(creates) == 1 and "--prerelease" in creates[0]
