//! 入口与协议握手回归：真实 NDJSON 帧、保留方法、视图方法与关闭后的资源回收。
//! @author kongweiguang
//!
//! 这些测试用内存双向流驱动与生产完全相同的 `entry::serve`，因此能覆盖此前只在真实
//! 宿主里才会暴露的问题：注册协议保留方法会让进程在握手前退出、SDK 的 `shutdown`
//! 只结束读取循环而不关闭插件自己的监听端口。
//!
//! 未覆盖：真实子进程的退出码、宿主收到 `shutdown` 回复后的强杀宽限期、标准输入
//! 被宿主直接关闭以外的异常（例如管道损坏）。这些需要真实宿主与操作系统配合，
//! 不能由本文件的断言代替。

use std::{sync::Arc, time::Duration};

use kerminal_plugin_lan_transfer::{
    config::{PLUGIN_ID, PLUGIN_VERSION},
    entry::{build_router, combine_outcome, serve},
    rpc::ViewRegistry,
    service::Controller,
    state::Phase,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

/// 写一行协议帧；测试不绕过 NDJSON 分帧，与宿主行为一致。
async fn send(writer: &mut tokio::io::WriteHalf<DuplexStream>, value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
}

/// 读取一帧；协议阻塞时让测试失败而不是永久挂起。
async fn receive_frame(reader: &mut BufReader<tokio::io::ReadHalf<DuplexStream>>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("protocol reply timed out")
        .expect("read reply");
    serde_json::from_str(&line).expect("valid NDJSON frame")
}

/// 有限等待读取一帧回复；主动 `view.update` 通知可能先于动作回执到达，必须跳过。
///
/// 协议允许通知与响应交错，失败动作现在会先推送内联错误再返回协议错误；若测试
/// 固定读取下一帧，就会把通知误当回执。只在看到带 `id` 的响应时返回。
async fn receive(reader: &mut BufReader<tokio::io::ReadHalf<DuplexStream>>) -> Value {
    loop {
        let value = receive_frame(reader).await;
        if value.get("id").is_some() {
            return value;
        }
    }
}

/// 读取动作回执，同时收集交错到达的通知，用于断言内联错误确实进了视图数据。
async fn receive_with_notifications(
    reader: &mut BufReader<tokio::io::ReadHalf<DuplexStream>>,
) -> (Value, Vec<Value>) {
    let mut notifications = Vec::new();
    loop {
        let value = receive_frame(reader).await;
        if value.get("id").is_some() {
            return (value, notifications);
        }
        notifications.push(value);
    }
}

/// 宿主握手参数；字段与 `InitializeParams` 完全一致（`deny_unknown_fields`）。
fn initialize(id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocol": 1,
            "host_api": "1.0.0",
            "plugin": { "id": PLUGIN_ID, "version": PLUGIN_VERSION },
            "locale": "zh-CN",
            "theme": "dark",
            "data_dir": "memory",
            "generation": 1
        }
    })
}

/// 让监听地址固定到回环，避免测试把临时目录暴露到局域网。
fn bind_loopback(controller: &Controller) {
    controller.state.select_address("127.0.0.1").unwrap();
}

/// 握手 → 视图快照 → 关闭：完整走一遍宿主实际会发的请求序列。
///
/// 这条用例直接覆盖「注册 `shutdown` 导致进程启动即失败」的回归：只要能收到
/// `initialize` 的回复，就说明路由注册成功、进程进入了协议循环。
#[tokio::test]
async fn handshake_serves_view_methods_and_shutdown_reclaims() {
    let controller = Controller::new();
    let (host, plugin) = tokio::io::duplex(16 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn({
        let controller = Arc::clone(&controller);
        async move { serve(&controller, plugin_read, plugin_write).await }
    });
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);

    send(&mut write, initialize("h:1")).await;
    let reply = receive(&mut read).await;
    assert_eq!(reply["id"], "h:1");
    assert_eq!(reply["result"]["protocol"], 1);
    assert_eq!(reply["result"]["plugin"]["id"], PLUGIN_ID);
    assert_eq!(reply["result"]["plugin"]["version"], PLUGIN_VERSION);

    // 宿主发来的通知（`initialized`）不产生回执，但必须被接受；否则后续请求
    // 会被 SDK 当成「未握手」拒绝。
    send(
        &mut write,
        json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await;
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:2","method":"view.snapshot","params":{}}),
    )
    .await;
    let reply = receive(&mut read).await;
    assert_eq!(reply["id"], "h:2");
    assert_eq!(reply["result"]["state_version"], 1);
    assert!(reply["result"]["state"]["selected_address"].is_string());
    // 恢复态只能带非敏感字段：链接、二维码与令牌不能经快照回到宿主草稿里。
    let state = reply["result"]["state"].as_object().unwrap();
    assert_eq!(state.len(), 2, "state: {state:?}");
    assert!(!state.contains_key("link") && !state.contains_key("qr"));

    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:3",
            "method":"view.open",
            "params":{"view":"lan.transfer.panel","instance":"w1","scope":{}}
        }),
    )
    .await;
    let reply = receive(&mut read).await;
    assert_eq!(reply["id"], "h:3");
    // 视图数据必须带阶段布尔量与计数文案，否则面板打开就是空白。
    assert_eq!(reply["result"]["data"]["running"], false);
    assert!(reply["result"]["data"]["stats"].is_string());
    // 新宿主据此渲染地址下拉；旧宿主忽略扩展，不影响原有输入框。
    assert!(reply["result"]["data"]["address_options"].is_array());

    // 动作在停止状态下不能静默成功：空目录是字段类型正确的输入错误，走私有
    // 输入码；同一本地化文案同时进入业务错误与视图数据，宿主据此只显示一次。
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:3a","method":"view.action",
               "params":{"instance":"w1","action":"start","data":{"root_path":""}}}),
    )
    .await;
    let (reply, notifications) = receive_with_notifications(&mut read).await;
    assert_eq!(reply["error"]["code"], -32104);
    let update = notifications
        .iter()
        .find(|notification| notification["method"] == "view.update")
        .expect("validation error must update the caller view");
    assert_eq!(update["params"]["instance"], "w1");
    assert_eq!(update["params"]["patch"]["show_error"], true);
    let inline = update["params"]["patch"]["error"]
        .as_str()
        .expect("inline validation error");
    assert!(!inline.is_empty());
    assert_eq!(reply["error"]["message"], inline);

    // 字段缺失属于线上形状错误，仍是 -32602，且不伪造内联文案。
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:3b","method":"view.action",
               "params":{"instance":"w1","action":"start","data":{}}}),
    )
    .await;
    let (reply, notifications) = receive_with_notifications(&mut read).await;
    assert_eq!(reply["error"]["code"], -32602);
    assert!(
        notifications.is_empty(),
        "malformed shape must not emit inline error: {notifications:?}"
    );

    // 未知方法走 SDK 的 method-not-found，而不是让进程退出。
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:4","method":"view.missing","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["error"]["code"], -32601);

    // 未知动作由插件自己拒绝，且错误码与参数错误一致（-32602），
    // 不能让它变成内部错误，否则界面会把用户操作显示成插件故障。
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:4a","method":"view.action",
               "params":{"instance":"w1","action":"nope","data":{}}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["error"]["code"], -32602);

    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:5","method":"shutdown","params":{}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["id"], "h:5");

    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("serve must return after shutdown")
        .expect("serve task must not panic");
    assert!(
        outcome.is_ok(),
        "shutdown path must report reclaimed: {outcome:?}"
    );
    // 关闭后状态必须回到 Idle；只回复宿主而不回收会在这里暴露。
    assert_eq!(controller.state.snapshot().unwrap().phase, Phase::Idle);
}

/// 协议保留方法不能由插件注册：`build_router` 成功后，再注册 `shutdown` 必须失败。
///
/// 这条断言固定住 SDK 的约束（`invalid or duplicate method`），避免以后有人为了
/// 「优雅关闭」再次注册同名方法，把进程挡在握手之前。
#[tokio::test]
async fn protocol_reserved_methods_cannot_be_registered() {
    let controller = Controller::new();
    let mut router = build_router(Arc::new(ViewRegistry::new(Arc::clone(&controller))))
        .expect("view methods register cleanly");
    for method in ["initialize", "initialized", "shutdown"] {
        let error = router
            .method(method, |_, _, _| async {
                Ok::<Value, kerminal_plugin_sdk::Error>(Value::Null)
            })
            .expect_err("reserved method must be rejected");
        assert_eq!(error.code, -32600, "method {method}");
    }
}

/// 输入流结束（宿主关闭标准输入）也必须回收：SDK 只结束读取循环，
/// 插件自己的监听端口与票据必须由入口负责关闭。
#[tokio::test]
async fn input_eof_still_reclaims_service() {
    let root = tempfile::tempdir().unwrap();
    let share = root.path().join("share");
    std::fs::create_dir(&share).unwrap();
    let controller = Controller::new();
    bind_loopback(&controller);
    let link = controller.start(share).await.unwrap();
    assert!(link.starts_with("http://127.0.0.1:"), "link: {link}");

    let (host, plugin) = tokio::io::duplex(4096);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn({
        let controller = Arc::clone(&controller);
        async move { serve(&controller, plugin_read, plugin_write).await }
    });
    // 关闭宿主端即输入 EOF；此时尚未握手，SDK 应直接结束循环。
    drop(host);

    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("serve must return on EOF")
        .expect("serve task must not panic");
    assert!(
        outcome.is_ok(),
        "EOF path must report reclaimed: {outcome:?}"
    );
    assert_eq!(controller.state.snapshot().unwrap().phase, Phase::Idle);
}

/// 协议失败与回收结果必须分别保留：任一项失败都不能被另一项掩盖。
///
/// 超时只在真实监听任务不响应时出现，无法在集成测试里稳定复现；这里直接断言
/// 映射本身，不声称已经跑过真实超时场景。
#[tokio::test]
async fn outcome_mapping_keeps_both_failures() {
    use std::io::{Error, ErrorKind};

    assert!(combine_outcome(Ok(()), true).is_ok());

    let timed_out = combine_outcome(Ok(()), false).unwrap_err();
    assert_eq!(timed_out.kind(), ErrorKind::TimedOut);

    let protocol =
        combine_outcome(Err(Error::new(ErrorKind::InvalidData, "bad frame")), true).unwrap_err();
    assert_eq!(protocol.kind(), ErrorKind::InvalidData);

    let both =
        combine_outcome(Err(Error::new(ErrorKind::InvalidData, "bad frame")), false).unwrap_err();
    assert_eq!(both.kind(), ErrorKind::InvalidData);
    assert!(both.to_string().contains("bad frame"));
    assert!(both.to_string().contains("shutdown timed out"));
}

/// 用户改完监听地址直接点「启动」时，宿主只把草稿放进动作数据，不会先发
/// `set-address`（`on_submit` 只在 Enter 触发）。启动必须采用这份快照，
/// 否则会按旧地址绑定，界面显示与实际监听不一致。
#[tokio::test]
async fn start_action_uses_snapshot_address_without_set_address() {
    let root = tempfile::tempdir().unwrap();
    let share = root.path().join("share");
    std::fs::create_dir(&share).unwrap();
    let controller = Controller::new();
    let (host, plugin) = tokio::io::duplex(16 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn({
        let controller = Arc::clone(&controller);
        async move { serve(&controller, plugin_read, plugin_write).await }
    });
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, initialize("h:1")).await;
    receive(&mut read).await;

    // 直接启动，动作数据里同时带目录与地址，不预先发 set-address。
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:2",
            "method":"view.action",
            "params":{
                "instance":"w1",
                "action":"start",
                "data":{
                    "root_path": share.to_str().unwrap(),
                    "selected_address": "127.0.0.1"
                }
            }
        }),
    )
    .await;
    let reply = receive(&mut read).await;
    assert!(reply["error"].is_null(), "start failed: {reply}");
    let patch = &reply["result"]["patch"];
    assert_eq!(patch["running"], true);
    assert_eq!(patch["selected_address"], "127.0.0.1");
    let address = patch["address"].as_str().expect("address");
    assert!(
        address.starts_with("127.0.0.1:"),
        "bound address must match the action snapshot: {address}"
    );

    // 无效地址必须明确失败，且不能退回旧地址继续启动。
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:3",
            "method":"view.action",
            "params":{"instance":"w1","action":"stop","data":{}}
        }),
    )
    .await;
    assert!(receive(&mut read).await["error"].is_null());
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:4",
            "method":"view.action",
            "params":{
                "instance":"w1",
                "action":"start",
                "data":{
                    "root_path": share.to_str().unwrap(),
                    "selected_address": "203.0.113.9"
                }
            }
        }),
    )
    .await;
    let (reply, notifications) = receive_with_notifications(&mut read).await;
    // 非本机地址是字段类型正确的输入错误：私有输入码，且内联文案与业务错误
    // 消息逐字一致，宿主才能抑制重复提示。
    assert_eq!(reply["error"]["code"], -32104, "{reply}");
    let update = notifications
        .iter()
        .find(|notification| notification["method"] == "view.update")
        .expect("address validation must update the caller view");
    assert_eq!(update["params"]["patch"]["show_error"], true);
    assert_eq!(
        reply["error"]["message"],
        update["params"]["patch"]["error"]
    );
    let snapshot = controller.state.snapshot().unwrap();
    assert_eq!(snapshot.selected_address, "127.0.0.1");
    assert_eq!(snapshot.phase, Phase::Idle);

    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:5","method":"shutdown","params":{}}),
    )
    .await;
    receive(&mut read).await;
    assert!(task.await.unwrap().is_ok());
}

/// 带空格边界的目录路径必须逐字使用：Linux/macOS 上 `"dir "` 与 `"dir"` 是两个
/// 不同目录，`trim()` 会把用户选的目录悄悄换成另一个。
///
/// 平台差异：Windows 不允许目录名以空格结尾，因此本用例只在 Unix 上创建这种目录；
/// Windows 上只断言「带内部空格的普通目录能正常启动」，不声称覆盖尾随空格。
#[tokio::test]
async fn start_keeps_directory_path_verbatim() {
    let controller = Controller::new();
    let (host, plugin) = tokio::io::duplex(16 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn({
        let controller = Arc::clone(&controller);
        async move { serve(&controller, plugin_read, plugin_write).await }
    });
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, initialize("h:1")).await;
    receive(&mut read).await;

    let base = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let name = "share ";
    #[cfg(windows)]
    let name = "share with spaces";
    let share = base.path().join(name);
    std::fs::create_dir(&share).unwrap();

    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:2",
            "method":"view.action",
            "params":{
                "instance":"w1",
                "action":"start",
                "data":{
                    "root_path": share.to_str().unwrap(),
                    "selected_address": "127.0.0.1"
                }
            }
        }),
    )
    .await;
    let reply = receive(&mut read).await;
    assert!(reply["error"].is_null(), "start failed: {reply}");
    // 面板展示的根名必须来自实际打开的目录，而不是被裁剪过的名字。
    assert_eq!(
        reply["result"]["patch"]["root_name"].as_str().unwrap(),
        name
    );

    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:3","method":"shutdown","params":{}}),
    )
    .await;
    receive(&mut read).await;
    assert!(task.await.unwrap().is_ok());
}

/// 通配地址必须由本运行期的显式动作选定：宿主旧草稿不能自动切换，
/// 显式 `set-address` 后快照与后续打开都保持一致。
#[tokio::test]
async fn wildcard_requires_explicit_selection_and_stays_in_runtime() {
    let controller = Controller::new();
    let (host, plugin) = tokio::io::duplex(16 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn({
        let controller = Arc::clone(&controller);
        async move { serve(&controller, plugin_read, plugin_write).await }
    });
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, initialize("h:1")).await;
    receive(&mut read).await;

    // 旧草稿里的通配值不能把新运行期自动切到全接口；下拉仍应提供该选项。
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:2",
            "method":"view.open",
            "params":{
                "view":"lan.transfer.panel",
                "instance":"w1",
                "scope":{},
                "state":{"selected_address":"0.0.0.0"}
            }
        }),
    )
    .await;
    let opened = receive(&mut read).await;
    assert!(opened["error"].is_null(), "{opened}");
    let data = &opened["result"]["data"];
    assert_ne!(
        data["selected_address"], "0.0.0.0",
        "stale wildcard must not be restored automatically"
    );
    let options = data["address_options"].as_array().unwrap();
    assert!(options.iter().any(|option| option["value"] == "0.0.0.0"));
    assert!(options.iter().any(|option| option["value"] == "127.0.0.1"));

    // 用户显式提交通配地址后才写入本次运行状态。
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:3",
            "method":"view.action",
            "params":{
                "instance":"w1",
                "action":"set-address",
                "data":{"selected_address":"0.0.0.0"}
            }
        }),
    )
    .await;
    let reply = receive(&mut read).await;
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(reply["result"]["patch"]["selected_address"], "0.0.0.0");

    // 快照是宿主恢复草稿的来源；本次运行期选过通配后应如实返回。
    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:4","method":"view.snapshot","params":{}}),
    )
    .await;
    let snapshot = receive(&mut read).await;
    assert_eq!(snapshot["result"]["state"]["selected_address"], "0.0.0.0");

    // 同一运行期再次打开视图时，通配值应保持而不是被默认地址覆盖。
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:5",
            "method":"view.open",
            "params":{
                "view":"lan.transfer.panel",
                "instance":"w2",
                "scope":{},
                "state":{"selected_address":"0.0.0.0"}
            }
        }),
    )
    .await;
    let reopened = receive(&mut read).await;
    assert_eq!(reopened["result"]["data"]["selected_address"], "0.0.0.0");

    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:6","method":"shutdown","params":{}}),
    )
    .await;
    receive(&mut read).await;
    assert!(task.await.unwrap().is_ok());
}

/// 通配运行期切换展示 IP：只更新 address/link/qr，不重启监听、不换端口或密钥。
///
/// 这里直接走真实协议动作，锁定面板依赖的 `wildcard` / `active_ip` /
/// `active_ip_options` 数据形状与错误码；剪贴板兜底路径由 e2e 覆盖。
#[tokio::test]
async fn wildcard_runtime_switches_active_ip_in_place() {
    let root = tempfile::tempdir().unwrap();
    let share = root.path().join("share");
    std::fs::create_dir(&share).unwrap();
    let controller = Controller::new();
    let (host, plugin) = tokio::io::duplex(16 * 1024);
    let (plugin_read, plugin_write) = tokio::io::split(plugin);
    let task = tokio::spawn({
        let controller = Arc::clone(&controller);
        async move { serve(&controller, plugin_read, plugin_write).await }
    });
    let (read, mut write) = tokio::io::split(host);
    let mut read = BufReader::new(read);
    send(&mut write, initialize("h:1")).await;
    receive(&mut read).await;

    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:2",
            "method":"view.action",
            "params":{
                "instance":"w1",
                "action":"start",
                "data":{
                    "root_path": share.to_str().unwrap(),
                    "selected_address": "0.0.0.0"
                }
            }
        }),
    )
    .await;
    let reply = receive(&mut read).await;
    assert!(reply["error"].is_null(), "wildcard start failed: {reply}");
    let patch = &reply["result"]["patch"];
    assert_eq!(patch["running"], true);
    assert_eq!(patch["wildcard"], true);
    assert_eq!(patch["single_address"], false);
    let options = patch["active_ip_options"].as_array().unwrap();
    assert!(
        !options.is_empty(),
        "wildcard run must expose address options"
    );
    let active_ip = patch["active_ip"].as_str().expect("active ip");
    assert_eq!(active_ip, options[0]["value"].as_str().unwrap());
    let address = patch["address"].as_str().expect("address");
    let port = address.rsplit(':').next().unwrap().parse::<u16>().unwrap();
    assert_eq!(address, format!("{active_ip}:{port}"));
    assert!(
        patch["link"]
            .as_str()
            .is_some_and(|link| link.starts_with(&format!("http://{active_ip}:{port}/#"))),
        "link must use the same active address and port"
    );
    assert!(options.iter().all(|option| {
        option["label"]
            .as_str()
            .unwrap()
            .contains(&format!(":{port}"))
    }));
    let qr_before = patch["qr"].clone();

    // 白名单外地址属于输入错误：不能悄悄切换或返回成功。
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:3",
            "method":"view.action",
            "params":{
                "instance":"w1",
                "action":"select-active-ip",
                "data":{"active_ip":"203.0.113.9"}
            }
        }),
    )
    .await;
    let (reply, _) = receive_with_notifications(&mut read).await;
    assert_eq!(reply["error"]["code"], -32104, "{reply}");
    assert_eq!(controller.state.snapshot().unwrap().active_ip, active_ip);

    // 选择当前地址是幂等动作：二维码与代次都不变。
    send(
        &mut write,
        json!({
            "jsonrpc":"2.0",
            "id":"h:4",
            "method":"view.action",
            "params":{
                "instance":"w1",
                "action":"select-active-ip",
                "data":{"active_ip": active_ip}
            }
        }),
    )
    .await;
    let reply = receive(&mut read).await;
    assert!(reply["error"].is_null(), "{reply}");
    assert_eq!(reply["result"]["patch"]["active_ip"], active_ip);
    assert_eq!(reply["result"]["patch"]["qr"], qr_before);

    // 有第二个地址时才可验证真实切换；测试机只有回环时这一分支自然不会执行。
    if options.len() >= 2 {
        let target = options.last().unwrap()["value"].as_str().unwrap();
        send(
            &mut write,
            json!({
                "jsonrpc":"2.0",
                "id":"h:5",
                "method":"view.action",
                "params":{
                    "instance":"w1",
                    "action":"select-active-ip",
                    "data":{"active_ip": target}
                }
            }),
        )
        .await;
        let reply = receive(&mut read).await;
        assert!(reply["error"].is_null(), "switch failed: {reply}");
        let patch = &reply["result"]["patch"];
        assert_eq!(patch["active_ip"], target);
        assert_eq!(patch["address"], format!("{target}:{port}"));
        assert_eq!(patch["wildcard"], true);
        assert_ne!(patch["qr"], qr_before, "qr must follow the switched link");
        assert!(
            patch["link"]
                .as_str()
                .is_some_and(|link| link.starts_with(&format!("http://{target}:{port}/#"))),
            "link must not keep the previous address"
        );
        let snapshot = controller.state.snapshot().unwrap();
        assert_eq!(snapshot.phase, Phase::Running);
        assert_eq!(snapshot.active_ip, target);
    }

    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:6","method":"view.action",
               "params":{"instance":"w1","action":"stop","data":{}}}),
    )
    .await;
    assert!(receive(&mut read).await["error"].is_null());
    // 停止后运行期网络字段必须立即清空。
    let stopped = controller.state.snapshot().unwrap();
    assert!(!stopped.wildcard);
    assert!(stopped.active_ip.is_empty());
    assert!(stopped.active_ip_options.is_empty());

    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":"h:7","method":"shutdown","params":{}}),
    )
    .await;
    receive(&mut read).await;
    assert!(task.await.unwrap().is_ok());
}
