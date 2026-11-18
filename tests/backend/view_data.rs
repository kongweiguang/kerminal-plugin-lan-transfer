//! 视图数据映射与视图文件契约回归：阶段布尔量、计数文案、空字段形状与声明式视图合法性。
//! @author kongweiguang
//!
//! 这些断言固定住 Sidecar 与 `resources/views/panel.json` 的字段契约：视图只能读取
//! 数据里的键，键名或形状变化必须同时反映到视图文件，否则面板会显示空白。视图文件
//! 还必须在打包前通过 SDK 的同一套校验，否则错误要到安装后才暴露。

use kerminal_plugin_lan_transfer::{
    rpc::data,
    state::{Phase, Snapshot},
};

/// 构造最小快照；只填与视图映射相关的字段，其余保持空值。
fn snapshot(phase: Phase, uploaded: u64, downloaded: u64) -> Snapshot {
    Snapshot {
        phase,
        root_path: None,
        address: None,
        link: None,
        root_name: None,
        qr: None,
        selected_address: "192.168.1.10".into(),
        addresses: vec!["192.168.1.10".into()],
        status: String::new(),
        error: None,
        uploaded,
        downloaded,
        started_at: None,
        expires_at: None,
    }
}

/// 运行中只点亮 `running`，其余阶段布尔量必须为假，避免按钮同时出现两个忙碌态。
#[test]
fn phase_flags_are_mutually_exclusive() {
    let running = data(&snapshot(Phase::Running, 0, 0));
    assert_eq!(running["running"], true);
    assert_eq!(running["starting"], false);
    assert_eq!(running["stopping"], false);

    let starting = data(&snapshot(Phase::Starting, 0, 0));
    assert_eq!(starting["running"], false);
    assert_eq!(starting["starting"], true);
}

/// 计数文案由 Sidecar 计算：视图没有拼接能力，空计数与有计数必须给出不同文案。
#[test]
fn stats_text_reflects_counters() {
    let empty = data(&snapshot(Phase::Idle, 0, 0));
    let stats = empty["stats"].as_str().unwrap();
    assert!(!stats.is_empty(), "stats must never be an empty string");

    let counted = data(&snapshot(Phase::Running, 3, 5));
    let stats = counted["stats"].as_str().unwrap();
    assert!(stats.contains('3') && stats.contains('5'), "stats: {stats}");
}

/// 未运行时不得携带链接、二维码或地址，面板据此隐藏扫码与复制入口。
#[test]
fn idle_snapshot_has_no_secrets() {
    let value = data(&snapshot(Phase::Idle, 0, 0));
    assert!(value["link"].is_null());
    assert!(value["qr"].is_null());
    assert!(value["address"].is_null());
    assert_eq!(value["selected_address"], "192.168.1.10");
    // 只给出回环时必须直接说明「仅本机可访问」，不能只列一个地址让用户误以为可用。
    assert!(
        value["addresses_hint"]
            .as_str()
            .is_some_and(|hint| !hint.is_empty()),
        "address hint must never be empty"
    );
}

/// 面板视图与数据契约必须自洽：每个 `$binding` 都能在默认数据里找到同名字段，
/// 每个 `%key%` 都由 `en.json` 覆盖。这条检查不替代 `kerminal-plugin check`，
/// 但能在不启动 CLI 的情况下先暴露改名遗漏。
#[test]
fn panel_view_bindings_match_default_data() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("resources/views/panel.json")).unwrap();
    let view: serde_json::Value = serde_json::from_str(&text).unwrap();
    let data = view["data"].as_object().expect("view data object");
    for binding in bindings(&text) {
        let head = binding.split('.').next().unwrap_or_default();
        assert!(data.contains_key(head), "missing default data: {binding}");
    }

    let english: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("resources/l10n/en.json")).unwrap())
            .unwrap();
    for key in translation_keys(&text) {
        assert!(
            english
                .get(&key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            "en.json missing key: {key}"
        );
    }
}

/// 抽取视图里的只读绑定名；`visible` 与 `qrcode.matrix` 等都必须来自实例数据。
fn bindings(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut index = 0;
    while let Some(offset) = text[index..].find("\"$") {
        let start = index + offset + 2;
        let end = start
            + text[start..]
                // 绑定名允许字母、数字、`_`、`-` 与层级分隔点，与宿主校验规则一致；
                // 少识别一个字符就会把 `$root_name` 截成 `root`，让检查静默失效。
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
                })
                .unwrap_or(text.len() - start);
        if end > start {
            names.push(text[start..end].to_owned());
        }
        index = end;
    }
    names
}

/// 抽取视图里的翻译键；未闭合的 `%` 直接忽略，避免把普通百分号当成键。
fn translation_keys(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut index = 0;
    while let Some(offset) = text[index..].find('%') {
        let start = index + offset + 1;
        let Some(end) = text[start..].find('%') else {
            break;
        };
        keys.push(text[start..start + end].to_owned());
        index = start + end + 1;
    }
    keys
}

/// 停止后视图必须显示可操作的原因，而不是空白错误行；
/// 这里用状态机构造一次真实失败路径，断言映射出的 `failed` 与文案。
#[test]
fn failed_phase_exposes_reason_and_hides_actions() {
    let state = kerminal_plugin_lan_transfer::state::State::new();
    state.begin_start().unwrap();
    state.mark_failed("请选择共享目录".into()).unwrap();
    let value = data(&state.snapshot().unwrap());
    assert_eq!(value["failed"], true);
    assert_eq!(value["running"], false);
    assert_eq!(value["starting"], false);
    assert_eq!(value["stopping"], false);
    assert_eq!(value["error"], "请选择共享目录");
    assert!(value["link"].is_null() && value["qr"].is_null() && value["address"].is_null());
}

/// 运行快照里的字段集合必须与视图默认数据一致：多出的字段视图读不到，
/// 缺失的字段会让绑定显示为空。这条断言不依赖 CLI，能在改字段名时立即失败。
#[test]
fn snapshot_fields_match_view_defaults() {
    let state = kerminal_plugin_lan_transfer::state::State::new();
    let runtime = data(&state.snapshot().unwrap());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("resources/views/panel.json")).unwrap();
    let view: serde_json::Value = serde_json::from_str(&text).unwrap();
    let defaults = view["data"].as_object().unwrap();
    let runtime = runtime.as_object().unwrap();
    // 视图默认数据里的每个键都必须由 Sidecar 提供，否则面板打开就是空白。
    for key in defaults.keys() {
        assert!(runtime.contains_key(key), "runtime data missing: {key}");
    }
    // 反向方向只允许「视图尚未绑定」的字段存在；这里显式列出，新增字段时必须
    // 同时决定是否要写进视图默认数据，而不是悄悄多出一个没人用的键。
    let unbound = runtime
        .keys()
        .filter(|key| !defaults.contains_key(*key))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        unbound,
        vec!["expires_at".to_owned()],
        "unexpected unbound keys"
    );
}
