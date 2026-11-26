//! 视图数据映射与视图文件契约回归：阶段布尔量、计数文案、空字段形状与声明式视图合法性。
//! @author kongweiguang
//!
//! 这些断言固定住 Sidecar 与 `resources/views/panel.json` 的字段契约：视图只能读取
//! 数据里的键，键名或形状变化必须同时反映到视图文件，否则面板会显示空白。视图文件
//! 还必须在打包前通过 SDK 的同一套校验，否则错误要到安装后才暴露。

use kerminal_plugin_lan_transfer::{
    rpc::data,
    state::{AddressOption, Phase, Snapshot},
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
        address_options: Vec::new(),
        status: String::new(),
        error: None,
        wildcard: false,
        single_address: true,
        active_ip: String::new(),
        active_ip_options: Vec::new(),
        address_items: Vec::new(),
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

/// 通配网络信息只在运行态出现：空闲时即使选中输入仍是 `0.0.0.0`，
/// `wildcard` 也必须为 false，避免非运行态泄露端口/密钥或让单地址行误显示。
#[test]
fn wildcard_network_fields_are_running_only() {
    let mut idle = snapshot(Phase::Idle, 0, 0);
    idle.selected_address = "0.0.0.0".into();
    let value = data(&idle, false);
    assert_eq!(value["wildcard"], false);
    assert_eq!(value["single_address"], true);
    assert_eq!(value["active_ip"], "");
    assert!(value["active_ip_options"].as_array().unwrap().is_empty());
}

/// 下拉需要分别保留显示标签和地址值，确保界面足以区分每种范围且动作能提交实际地址。
#[test]
fn address_options_expose_loopback_and_wildcard() {
    let mut snapshot = snapshot(Phase::Idle, 0, 0);
    snapshot.address_options = vec![
        AddressOption {
            label: "192.168.1.10 · WLAN".into(),
            value: "192.168.1.10".into(),
        },
        AddressOption {
            label: "127.0.0.1 · This device only".into(),
            value: "127.0.0.1".into(),
        },
        AddressOption {
            label: "0.0.0.0 · All IPv4 adapters".into(),
            value: "0.0.0.0".into(),
        },
    ];
    let value = data(&snapshot, false);
    let options = value["address_options"].as_array().unwrap();
    assert_eq!(options.len(), 3);
    for option in options {
        assert!(!option["label"].as_str().unwrap().is_empty());
        assert!(!option["value"].as_str().unwrap().is_empty());
    }
    assert!(
        options.iter().any(|option| option["value"] == "0.0.0.0"
            && option["label"].as_str().unwrap().contains("IPv4")),
        "{options:?}"
    );
    assert!(
        options.iter().any(|option| option["value"] == "127.0.0.1"
            && option["label"].as_str().unwrap().contains("127.0.0.1")),
        "{options:?}"
    );
}

/// 通配运行必须暴露当前 IP、带端口的选项与单地址布尔量；单地址运行不暴露
/// 可切换集合，视图据此渲染等宽地址文本而不是下拉。
#[test]
fn wildcard_running_exposes_switchable_active_ip() {
    let mut wildcard = snapshot(Phase::Running, 0, 0);
    wildcard.selected_address = "0.0.0.0".into();
    wildcard.address = Some("192.168.1.10:43210".into());
    wildcard.link = Some("http://192.168.1.10:43210/#secret".into());
    wildcard.wildcard = true;
    wildcard.single_address = false;
    wildcard.active_ip = "192.168.1.10".into();
    wildcard.active_ip_options = vec![
        AddressOption {
            label: "192.168.1.10:43210 · Ethernet".into(),
            value: "192.168.1.10".into(),
        },
        AddressOption {
            label: "127.0.0.1:43210 · 仅本机".into(),
            value: "127.0.0.1".into(),
        },
    ];
    let value = data(&wildcard, true);
    assert_eq!(value["wildcard"], true);
    assert_eq!(value["single_address"], false);
    assert_eq!(value["active_ip"], "192.168.1.10");
    let options = value["active_ip_options"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    assert!(
        options
            .iter()
            .all(|option| option["label"].as_str().unwrap().contains(":43210"))
    );

    let mut single = snapshot(Phase::Running, 0, 0);
    single.address = Some("192.168.1.10:43210".into());
    single.wildcard = false;
    single.single_address = true;
    let value = data(&single, false);
    assert_eq!(value["wildcard"], false);
    assert_eq!(value["single_address"], true);
    assert_eq!(value["active_ip"], "");
    assert!(value["active_ip_options"].as_array().unwrap().is_empty());
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
    // 标题直接是 Header 的第一个可见子项；下面不再有「未启动/运行中」副标题。
    assert_eq!(header["children"][0]["x-kerminal-appearance"], "title");
    assert!(!text.contains("\"$status\""));
    assert!(!text.contains("\"type\": \"badge\""));
    // 常驻提示已移除，只保留真实错误行。
    assert!(!text.contains("\"type\": \"notice\""));
    assert!(!text.contains("%notice.plaintext%"));
    let error_index = children
        .iter()
        .position(|node| node["text"] == "$error")
        .expect("actionable error row");
    assert_eq!(children[error_index]["tone"], "danger");
    assert_eq!(children[error_index]["visible"], "$show_error");
    // 运行卡片是无标题的圆角分组，内部按「目录 / 访问地址 / 二维码 / 计数」排列。
    let running_section = children
        .iter()
        .find(|node| node["type"] == "section" && node["visible"] == "$running")
        .expect("running section");
    assert_eq!(running_section["title"], "");
    let running_children = running_section["children"].as_array().unwrap();
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
    // 一次性配对密钥不能作为普通文本出现；复制必须走平铺行动作。
    assert!(!text.contains("\"$link\""));
    assert!(text.contains("\"x-kerminal-row-action\": \"copy-link\""));
    // 地址输入保持原绑定与 Enter 提交，同时用扩展声明下拉选项；旧宿主忽略扩展。
    assert!(text.contains("\"bind\": \"selected_address\""));
    assert!(text.contains("\"on_submit\": \"set-address\""));
    assert!(text.contains("\"x-kerminal-select-options\": \"$address_options\""));
    // 运行区两个分组块：目录（标签 + 完整路径）与访问地址（标签 + 平铺逐行复制列表）。
    let folder_block = running_children
        .iter()
        .find(|node| node["type"] == "stack" && node["children"][0]["text"] == "%field.folder%")
        .expect("folder block");
    assert_eq!(
        folder_block["children"][0]["x-kerminal-appearance"],
        "label"
    );
    assert_eq!(folder_block["children"][1]["text"], "$shared_path");
    assert_eq!(
        folder_block["children"][1]["x-kerminal-appearance"],
        "value"
    );
    let address_block = running_children
        .iter()
        .find(|node| node["type"] == "stack" && node["children"][0]["text"] == "%field.address%")
        .expect("address block");
    assert_eq!(
        address_block["children"][0]["x-kerminal-appearance"],
        "label"
    );
    let address_list = &address_block["children"][1];
    assert_eq!(address_list["type"], "list");
    assert_eq!(address_list["items"], "$address_items");
    assert_eq!(address_list["key"], "ip");
    assert_eq!(address_list["title"], "address");
    assert_eq!(address_list["subtitle"], "group");
    assert_eq!(address_list["on_activate"], "select-active-ip");
    assert_eq!(address_list["x-kerminal-appearance"], "flat-copy-list");
    assert_eq!(address_list["x-kerminal-row-action"], "copy-link");
    assert_eq!(address_list["x-kerminal-selected-binding"], "$active_ip");
    assert_eq!(address_list["x-kerminal-copied-binding"], "$copied_ip");
    assert!(
        running_children
            .iter()
            .any(|node| node["type"] == "qrcode" && node["matrix"] == "$qr")
    );
    assert!(
        running_children
            .iter()
            .any(|node| node["text"] == "$stats" && node["tone"] == "muted")
    );
    // 复制确认只通过行尾图标高亮 `$copied_ip` 表达，不再插入推动二维码的提示行。
    assert!(!text.contains("\"$copy_status\""));
    assert!(!text.contains("\"$copied\""));
    // 旧的长说明、范围行与编辑区提示都已移除，不留下无绑定的死节点。
    assert!(!text.contains("$address_note"));
    assert!(!text.contains("$scope"));
    assert!(!text.contains("$addresses_hint"));
    assert!(!text.contains("%section.share%"));
    // 运行中的路径必须读取只读运行时字段，不能绑定到可被宿主草稿保护的 root_path。
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
    // 运行期网络字段不得在空闲态泄露：视图据此只显示编辑卡片。
    assert_eq!(value["wildcard"], false);
    assert_eq!(value["single_address"], true);
    assert_eq!(value["active_ip"], "");
    assert!(value["active_ip_options"].as_array().unwrap().is_empty());
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
