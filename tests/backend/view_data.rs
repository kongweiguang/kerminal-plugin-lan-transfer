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
    let running = data(&snapshot(Phase::Running, 0, 0), false);
    assert_eq!(running["running"], true);
    assert_eq!(running["starting"], false);
    assert_eq!(running["stopping"], false);

    let starting = data(&snapshot(Phase::Starting, 0, 0), false);
    assert_eq!(starting["running"], false);
    assert_eq!(starting["starting"], true);
}

/// 编辑入口与动作按钮必须随阶段切换：运行/启动/停止期间不能改目录或地址，
/// 否则界面输入会与真实监听不一致；动作按钮则要留在原位显示忙碌态——启动中
/// 继续显示 Start，停止中显示 Stop，避免面板在过渡期突然换按钮造成跳变。
/// 空闲/失败态的停止入口只在控制器仍保留可回收句柄时出现。
#[test]
fn phase_controls_editing_and_actions() {
    let idle = data(&snapshot(Phase::Idle, 0, 0), false);
    assert_eq!(idle["editable"], true);
    assert_eq!(idle["can_start"], true);
    assert_eq!(idle["can_stop"], false);
    // 取消启动可能留下未确认退出的孤儿句柄；空闲态仍需停止入口回收。
    assert_eq!(data(&snapshot(Phase::Idle, 0, 0), true)["can_stop"], true);

    // 启动中 Start 仍然可见并处于 busy；此时不显示 Stop，防止用户误以为
    // 已经可以回收一个尚未建立的服务。
    let starting = data(&snapshot(Phase::Starting, 0, 0), false);
    assert_eq!(starting["editable"], false);
    assert_eq!(starting["can_start"], true);
    assert_eq!(starting["can_stop"], false);

    let running = data(&snapshot(Phase::Running, 0, 0), false);
    assert_eq!(running["editable"], false);
    assert_eq!(running["can_start"], false);
    assert_eq!(running["can_stop"], true);

    // 停止中 Stop 仍留在原位并处于 busy；不再显示 Start，避免重复启动。
    let stopping = data(&snapshot(Phase::Stopping, 0, 0), false);
    assert_eq!(stopping["editable"], false);
    assert_eq!(stopping["can_start"], false);
    assert_eq!(stopping["can_stop"], true);

    // 普通启动失败（目录缺失、绑定失败）没有运行句柄，不应再显示 Stop。
    let failed_without_handle = data(&snapshot(Phase::Failed, 0, 0), false);
    assert_eq!(failed_without_handle["editable"], true);
    assert_eq!(failed_without_handle["can_start"], true);
    assert_eq!(failed_without_handle["can_stop"], false);

    // 停止失败或取消启动可能仍保留句柄，失败态必须保留 Stop 作为回收路径。
    let failed_with_handle = data(&snapshot(Phase::Failed, 0, 0), true);
    assert_eq!(failed_with_handle["editable"], true);
    assert_eq!(failed_with_handle["can_start"], true);
    assert_eq!(failed_with_handle["can_stop"], true);
}

/// 复制按钮默认显示「复制链接」：复制成功只切换按钮文案，不额外插入提示行，
/// 因此不会把二维码向下挤动，也不依赖延时或定时器。
#[test]
fn copy_confirmation_is_hidden_by_default() {
    let mut running = snapshot(Phase::Running, 0, 0);
    running.link = Some("http://127.0.0.1:43210/#secret".into());
    running.root_path = Some("C:/share/folder".into());
    let running = data(&running, true);
    assert!(
        running["copy_label"]
            .as_str()
            .is_some_and(|label| !label.is_empty()),
        "copy label must never be empty"
    );
    assert_eq!(running["show_error"], false);
    // 链接仍保留在 RPC 快照里供剪贴板动作与集成测试使用，但不作为面板文本展示。
    assert!(running["link"].as_str().is_some());
    // 运行展示读取只读的 shared_path；root_path 仍服务于可编辑草稿。
    assert_eq!(running["shared_path"], "C:/share/folder");
    assert_eq!(running["root_path"], "C:/share/folder");
}

/// 运行展示只去掉可确定形态的 Windows verbatim 前缀，原始 `root_path` 必须原样保留：
/// 生命周期与持久化依赖内核长路径形式，展示字符串不能写回状态。设备命名空间
/// 去掉前缀会变成误导性的相对路径，因此必须原样显示。
#[test]
fn shared_path_strips_verbatim_prefix_for_display_only() {
    let mut drive = snapshot(Phase::Running, 0, 0);
    drive.root_path = Some(r"\\?\C:\share\folder".into());
    let drive = data(&drive, false);
    assert_eq!(drive["shared_path"], r"C:\share\folder");
    assert_eq!(drive["root_path"], r"\\?\C:\share\folder");

    let mut unc = snapshot(Phase::Running, 0, 0);
    unc.root_path = Some(r"\\?\UNC\server\share\folder".into());
    let unc = data(&unc, false);
    assert_eq!(unc["shared_path"], r"\\server\share\folder");
    assert_eq!(unc["root_path"], r"\\?\UNC\server\share\folder");

    // 卷 GUID 路径没有可读的盘符替代形式：保留前缀比显示成 `Volume{...}\`
    // 更诚实，后者会被误读为普通相对目录。
    let mut volume = snapshot(Phase::Running, 0, 0);
    volume.root_path = Some(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\share".into());
    let volume = data(&volume, false);
    assert_eq!(
        volume["shared_path"],
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\share"
    );
    assert_eq!(
        volume["root_path"],
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\share"
    );

    let mut plain = snapshot(Phase::Running, 0, 0);
    plain.root_path = Some("C:/share/folder".into());
    assert_eq!(data(&plain, false)["shared_path"], "C:/share/folder");
}

/// 范围提示必须来自实际绑定地址：回环只说「仅本机」，不能写成手机可访问；
/// 局域网地址才提示同一网络。地址缺失时保持空串，避免显示错误范围。
#[test]
fn scope_follows_bound_address() {
    use kerminal_plugin_lan_transfer::l10n::{Key, Locale};

    let locale = Locale::detect();
    let mut loopback = snapshot(Phase::Running, 0, 0);
    loopback.address = Some("127.0.0.1:43210".into());
    assert_eq!(
        data(&loopback, false)["scope"],
        locale.text(Key::ScopeLocalOnly)
    );

    let mut lan = snapshot(Phase::Running, 0, 0);
    lan.address = Some("192.168.1.10:43210".into());
    assert_eq!(data(&lan, false)["scope"], locale.text(Key::ScopeLan));

    assert_eq!(data(&snapshot(Phase::Running, 0, 0), false)["scope"], "");
}

/// 地址提示把回环说明另起一行：地址列表与限制说明不能挤成带行首分号的
/// 一整段，否则长列表换行后会出现悬空标点。
#[test]
fn address_hint_puts_loopback_note_on_its_own_line() {
    let mut snapshot = snapshot(Phase::Idle, 0, 0);
    snapshot.addresses = vec!["127.0.0.1".into(), "192.168.1.10".into()];
    let hint = data(&snapshot, false)["addresses_hint"]
        .as_str()
        .unwrap()
        .to_owned();
    let note = hint
        .split('\n')
        .nth(1)
        .expect("loopback note on a new line");
    assert!(note.contains("127.0.0.1"), "{hint}");
    assert!(!note.starts_with(';'), "{hint}");
    assert!(!hint.contains("；"), "{hint}");
}

/// 面板外观契约：根滚动声明 inset，标题/操作行声明 header，分组区声明 grouped。
/// 旧宿主会忽略 `x-` 扩展，仍然只按节点语义渲染；这里锁定新宿主的接入点。
#[test]
fn panel_declares_opt_in_appearance() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("resources/views/panel.json")).unwrap();
    let view: serde_json::Value = serde_json::from_str(&text).unwrap();
    let scroll = &view["root"];
    assert_eq!(scroll["type"], "scroll");
    assert_eq!(scroll["x-kerminal-appearance"], "inset");
    // 动作错误通过视图数据展示：宿主只在当前动作提供内联错误时使用一次，
    // 不再叠加通用错误提示；旧宿主忽略该扩展并按协议错误回退。
    assert_eq!(scroll["x-kerminal-action-error"], "$error");

    let children = scroll["child"]["children"].as_array().unwrap();
    let header = children
        .iter()
        .find(|node| node["x-kerminal-appearance"] == "header")
        .expect("header row");
    assert_eq!(header["type"], "row");
    assert_eq!(
        header["children"][0]["children"][0]["x-kerminal-appearance"],
        "title"
    );
    // 状态是低优先级的语义文本，用 caption 与标题区分层级。
    assert_eq!(
        header["children"][0]["children"][1]["x-kerminal-appearance"],
        "caption"
    );
    let notice = children
        .iter()
        .find(|node| node["type"] == "notice")
        .expect("trusted-network notice");
    assert_eq!(notice["x-kerminal-appearance"], "caption");
    // 可操作的具体原因必须排在通用安全提醒之前：用户先看到该改什么，
    // 而不是先读一段与本次失败无关的说明。
    let error_index = children
        .iter()
        .position(|node| node["text"] == "$error")
        .expect("actionable error row");
    let notice_index = children
        .iter()
        .position(|node| node["type"] == "notice")
        .unwrap();
    assert!(error_index < notice_index, "error must precede notice");
    // 运行区标题已经是目录名，下面不能再重复一个「目录」标签；
    // 同时范围提示必须绑定实际绑定地址（$scope），不能写成固定的局域网文案。
    let running_section = children
        .iter()
        .find(|node| node["title"] == "$root_name")
        .expect("running section");
    let running_children = running_section["children"].as_array().unwrap();
    assert!(
        !running_children
            .iter()
            .any(|node| node["text"] == "%field.folder%"),
        "running section must not repeat the folder label"
    );
    assert!(
        running_children.iter().any(|node| node["text"] == "$scope"),
        "running section must show the bound scope"
    );
    // 启动/停止是兄弟节点，各自用阶段布尔量控制可见性；两者不会同时显示忙碌态。
    let actions = header["children"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["action"].as_str())
        .collect::<Vec<_>>();
    assert!(actions.contains(&"start"));
    assert!(actions.contains(&"stop"));
    assert!(header["children"].as_array().unwrap().iter().any(|node| {
        node["action"] == "start" && node["x-kerminal-requires-text"] == "$root_path"
    }));
    assert_eq!(
        children
            .iter()
            .filter(|node| node["x-kerminal-appearance"] == "grouped")
            .count(),
        2
    );
    // 一次性配对密钥不能作为普通文本出现；复制必须走按钮动作。
    assert!(!text.contains("\"$link\""));
    assert!(text.contains("\"action\": \"copy-link\""));
    // 复制结果只改按钮文案，不能再插入一条会推动二维码的独立提示行。
    assert!(text.contains("\"label\": \"$copy_label\""));
    assert!(!text.contains("\"$copy_status\""));
    assert!(!text.contains("\"$copied\""));
    // 运行中的路径必须读取只读运行时字段，不能绑定到可被宿主草稿保护的 root_path。
    assert!(
        running_children
            .iter()
            .any(|node| node["text"] == "$shared_path")
    );
    assert!(
        running_children
            .iter()
            .all(|node| node["text"] != "$root_path")
    );
}

/// 计数文案由 Sidecar 计算：视图没有拼接能力，空计数与有计数必须给出不同文案。
#[test]
fn stats_text_reflects_counters() {
    let empty = data(&snapshot(Phase::Idle, 0, 0), false);
    let stats = empty["stats"].as_str().unwrap();
    assert!(!stats.is_empty(), "stats must never be an empty string");

    let counted = data(&snapshot(Phase::Running, 3, 5), false);
    let stats = counted["stats"].as_str().unwrap();
    assert!(stats.contains('3') && stats.contains('5'), "stats: {stats}");
}

/// 未运行时不得携带链接、二维码或地址，面板据此隐藏扫码与复制入口。
#[test]
fn idle_snapshot_has_no_secrets() {
    let value = data(&snapshot(Phase::Idle, 0, 0), false);
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
    let value = data(&state.snapshot().unwrap(), false);
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
    let runtime = data(&state.snapshot().unwrap(), false);
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
