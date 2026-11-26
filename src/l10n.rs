//! 视图与状态文案的本地化。
//! @author kongweiguang
//!
//! SDK 的初始化参数不向插件暴露宿主语言，因此这里按操作系统区域选择中/英两种文案。
//! 只保留两套完整字典而不是逐条回退，是为了避免出现「一半中文一半英文」的界面。

use std::sync::OnceLock;

/// 支持的语言；未识别的一律用英文，避免用中文冒充国际化。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Locale {
    Zh,
    En,
}

impl Locale {
    /// 首次调用时按系统区域确定；后续调用复用结果，避免每个请求都读环境。
    pub fn detect() -> Self {
        static CACHED: OnceLock<Locale> = OnceLock::new();
        *CACHED.get_or_init(|| {
            let tag = sys_locale::get_locale()
                .or_else(|| std::env::var("LANG").ok())
                .unwrap_or_default()
                .to_lowercase();
            if tag.starts_with("zh") {
                Self::Zh
            } else {
                Self::En
            }
        })
    }

    /// 文案查找；缺失键返回英文占位而不是空串，便于发现漏配。
    pub fn text(self, key: Key) -> &'static str {
        let (zh, en) = key.pair();
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

/// Sidecar 侧动态文案键。静态文案由视图文件的 `%key%` 与 `resources/l10n/*.json`
/// 提供；这里只覆盖必须由进程计算的内容：状态行、失败原因与传输计数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    StatusIdle,
    StatusStarting,
    StatusRunning,
    StatusStopping,
    StatusFailed,
    AddressOptionLoopback,
    AddressOptionAllInterfaces,
    ActiveIpLoopback,
    ErrorRootMissing,
    ErrorRootNotDirectory,
    ErrorRootRejected,
    ErrorAddressInvalid,
    ErrorAddressNotLocal,
    ErrorNotRunning,
    ErrorCopyLinkChanged,
    ErrorBind,
    ErrorBusy,
    ErrorStopTimeout,
    ErrorInternal,
    Stats,
    StatsEmpty,
    CopyAction,
    CopyDone,
}

impl Key {
    /// 中英文案成对声明，避免两套字典分别维护后错位。
    fn pair(self) -> (&'static str, &'static str) {
        match self {
            Self::StatusIdle => ("未启动", "Not running"),
            Self::StatusStarting => ("正在启动…", "Starting…"),
            Self::StatusRunning => ("运行中", "Running"),
            Self::StatusStopping => ("正在停止…", "Stopping…"),
            // Failed 也可能来自停止失败；状态徽标不能写成「启动失败」，否则用户
            // 会把停止回收故障误判成启动问题。具体原因由下方错误行给出。
            Self::StatusFailed => ("操作失败", "Failed"),
            // 下拉选项明确写出地址和监听范围，便于分辨回环和通配。
            Self::AddressOptionLoopback => ("127.0.0.1 · 仅本机", "127.0.0.1 · This device only"),
            Self::AddressOptionAllInterfaces => {
                ("0.0.0.0 · 所有 IPv4 网卡", "0.0.0.0 · All IPv4 adapters")
            }
            // 通配运行期的回环选项：明确「仅本机」，避免用户把二维码发给外部设备。
            Self::ActiveIpLoopback => ("仅本机", "This device only"),
            Self::ErrorRootMissing => ("请选择共享目录", "Choose a shared folder"),
            Self::ErrorRootNotDirectory => ("所选路径不是目录", "Selected path is not a folder"),
            Self::ErrorRootRejected => (
                "该目录不可共享（隐藏目录、链接或无法安全打开）",
                "This folder cannot be shared (hidden, linked, or unsafe to open)",
            ),
            Self::ErrorAddressInvalid => ("监听地址无效", "Invalid listen address"),
            // 格式正确但不属于本机网卡，与「格式无效」分开说明；否则用户会反复
            // 检查拼写，却不知道这个地址根本不能在本机监听。
            Self::ErrorAddressNotLocal => (
                "该地址不属于本机",
                "That address does not belong to this computer",
            ),
            Self::ErrorNotRunning => ("服务未运行", "Service is not running"),
            Self::ErrorCopyLinkChanged => (
                "共享链接已失效，请复制当前链接",
                "The copied link is no longer valid. Copy the current link.",
            ),
            Self::ErrorBind => ("无法绑定该地址与端口", "Cannot bind that address and port"),
            Self::ErrorBusy => ("服务正在切换状态，请稍候", "Service is busy, try again"),
            // 停止超时与普通内部错误不同：监听句柄可能仍未回收，用户应重试停止
            // 而不是以为服务已正常结束。
            Self::ErrorStopTimeout => ("停止超时，请重试", "Stopping timed out; try again"),
            Self::ErrorInternal => ("内部错误，请重试", "Internal error, try again"),
            Self::Stats => (
                "本次已上传 {up} 个、已下载 {down} 个文件",
                "Uploaded {up}, downloaded {down}",
            ),
            Self::StatsEmpty => ("本次还没有传输", "No transfers yet"),
            Self::CopyAction => ("复制链接", "Copy link"),
            Self::CopyDone => ("已复制", "Copied"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两套字典必须逐键成对且都不为空：任何一边留空都会让界面出现空白行，
    /// 而空白行正是「状态区随阶段跳动」的常见来源。
    #[test]
    fn every_key_has_both_languages() {
        let keys = [
            Key::StatusIdle,
            Key::StatusStarting,
            Key::StatusRunning,
            Key::StatusStopping,
            Key::StatusFailed,
            Key::AddressOptionLoopback,
            Key::AddressOptionAllInterfaces,
            Key::ActiveIpLoopback,
            Key::ErrorRootMissing,
            Key::ErrorRootNotDirectory,
            Key::ErrorRootRejected,
            Key::ErrorAddressInvalid,
            Key::ErrorAddressNotLocal,
            Key::ErrorNotRunning,
            Key::ErrorCopyLinkChanged,
            Key::ErrorBind,
            Key::ErrorBusy,
            Key::ErrorStopTimeout,
            Key::ErrorInternal,
            Key::Stats,
            Key::StatsEmpty,
            Key::CopyAction,
            Key::CopyDone,
        ];
        for key in keys {
            let (zh, en) = key.pair();
            assert!(!zh.trim().is_empty(), "{key:?} zh");
            assert!(!en.trim().is_empty(), "{key:?} en");
        }
    }

    /// 计数文案必须同时包含两个占位符：缺一个就会让界面显示 `{down}` 原文。
    #[test]
    fn stats_text_keeps_both_placeholders() {
        let (zh, en) = Key::Stats.pair();
        for text in [zh, en] {
            assert!(text.contains("{up}"), "{text}");
            assert!(text.contains("{down}"), "{text}");
        }
    }

    /// 静态文案的键与视图文件里的 `%key%` 必须一一对应，中英两份都不能漏；
    /// 漏键会让面板直接显示 `%field.folder%` 这样的原始文本。
    #[test]
    fn static_keys_match_view_and_both_locales() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let view = std::fs::read_to_string(root.join("resources/views/panel.json")).unwrap();
        let english: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("resources/l10n/en.json")).unwrap())
                .unwrap();
        let chinese: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("resources/l10n/zh-CN.json")).unwrap())
                .unwrap();
        let mut used = Vec::new();
        let mut rest = view.as_str();
        while let Some(start) = rest.find('%') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('%') else {
                break;
            };
            used.push(after[..end].to_owned());
            rest = &after[end + 1..];
        }
        assert!(!used.is_empty(), "view must reference l10n keys");
        for key in used {
            for (name, table) in [("en", &english), ("zh-CN", &chinese)] {
                assert!(
                    table
                        .get(&key)
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty()),
                    "{name}.json missing key: {key}"
                );
            }
        }
        // 两份字典的键集合必须完全一致：只加英文会让中文界面回退成键名。
        let mut english_keys = english
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut chinese_keys = chinese
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        english_keys.sort();
        chinese_keys.sort();
        assert_eq!(english_keys, chinese_keys);
    }
}
