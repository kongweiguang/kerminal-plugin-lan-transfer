//! 虚拟相对路径：把浏览器输入收敛为受限的段列表，绝不拼接绝对路径。
//! @author kongweiguang
//!
//! 浏览器只传不透明 id；本模块负责把 id 解析成「相对共享根的段列表」，并拒绝
//! 绝对路径、盘符、UNC、`..`、控制字符、Windows 保留名、ADS 冒号和隐藏项。
//! 之所以在此处做完整校验而不是依赖后续文件系统调用，是因为 Windows 路径别名太多，
//! 任何一个漏网别名都会让「共享根」形同虚设。

use crate::{
    config::{MAX_NAME_BYTES, MAX_PATH_COMPONENTS, TEMP_PREFIX},
    error::{AppError, AppResult},
};

/// 共享根固定 id；页面据此判断面包屑首项，服务端也用它拒绝其他根别名。
pub const ROOT_ID: &str = "root";

/// 已校验的虚拟路径：空段列表就是根。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualPath {
    segments: Vec<String>,
}

impl VirtualPath {
    /// 根路径只由固定 id 生成，不接受调用方拼接。
    pub fn root() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    /// 解析浏览器给出的 id。`root` 代表共享根，其余形式都是 `root/段/段`。
    pub fn parse(id: &str) -> AppResult<Self> {
        if id == ROOT_ID {
            return Ok(Self::root());
        }
        let rest = id
            .strip_prefix(ROOT_ID)
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or_else(AppError::rejected)?;
        if rest.is_empty() {
            return Err(AppError::rejected());
        }
        let mut segments = Vec::new();
        for segment in rest.split('/') {
            validate_segment(segment)?;
            segments.push(segment.to_owned());
        }
        if segments.len() > MAX_PATH_COMPONENTS {
            return Err(AppError::rejected());
        }
        Ok(Self { segments })
    }

    /// 从已校验的段构建，用于在已知安全的父目录后追加新名字。
    pub fn from_segments(segments: Vec<String>) -> AppResult<Self> {
        if segments.len() > MAX_PATH_COMPONENTS {
            return Err(AppError::rejected());
        }
        for segment in &segments {
            validate_segment(segment)?;
        }
        Ok(Self { segments })
    }

    /// 追加一个已校验子段，用于进入子目录。
    pub fn join(&self, segment: &str) -> AppResult<Self> {
        validate_segment(segment)?;
        if self.segments.len() >= MAX_PATH_COMPONENTS {
            return Err(AppError::rejected());
        }
        let mut segments = self.segments.clone();
        segments.push(segment.to_owned());
        Ok(Self { segments })
    }

    /// 去掉最后一段，用于从文件定位父目录。
    pub fn parent(&self) -> Self {
        let mut segments = self.segments.clone();
        segments.pop();
        Self { segments }
    }

    /// 供文件系统逐段打开的段列表；空列表表示共享根本身。
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// 不透明 id 由段列表编码；页面只原样回传，不解析内部结构。
    pub fn id(&self) -> String {
        if self.segments.is_empty() {
            ROOT_ID.to_owned()
        } else {
            format!("{ROOT_ID}/{}", self.segments.join("/"))
        }
    }

    /// 展示名只用于面包屑，绝不回显本机绝对路径。
    pub fn display_name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// 判断是否位于内部暂存区或隐藏项之下；列表与直接访问共用同一规则。
    pub fn is_restricted(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| is_restricted_segment(segment))
    }
}

/// 单个路径段的校验；隐藏项、内部暂存和 Windows 别名都在这里统一拒绝。
pub fn validate_segment(segment: &str) -> AppResult<()> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.len() > MAX_NAME_BYTES
        || segment
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':' | '\0'))
        || segment.ends_with([' ', '.'])
        || is_reserved_name(segment)
        || is_restricted_segment(segment)
    {
        return Err(AppError::rejected());
    }
    Ok(())
}

/// 隐藏项（点开头）与内部暂存前缀一律不可见、不可直接访问。
///
/// 这里不区分「是否由本插件创建」：共享目录里任何点开头的名字都可能承载用户隐私，
/// 一律不共享比逐个白名单更不容易出错。
pub fn is_restricted_segment(segment: &str) -> bool {
    segment.starts_with('.') || segment.starts_with(TEMP_PREFIX)
}

/// Windows 保留设备名不区分大小写，且允许带扩展名，必须按主干判断。
fn is_reserved_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or_default().to_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

/// 上传文件名来自浏览器，先做同一套段校验再用于暂存与提交。
pub fn validate_upload_name(name: &str) -> AppResult<String> {
    validate_segment(name)?;
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合法根与子路径必须能往返编码，否则页面刷新会丢失当前目录。
    #[test]
    fn round_trip_ids() {
        let root = VirtualPath::parse("root").unwrap();
        assert_eq!(root.id(), "root");
        let nested = root.join("2026-11").unwrap().join("reports").unwrap();
        assert_eq!(nested.id(), "root/2026-11/reports");
        assert_eq!(VirtualPath::parse(&nested.id()).unwrap(), nested);
    }

    /// 常见逃逸写法必须全部拒绝，避免把校验责任下放给文件系统。
    #[test]
    fn rejects_escapes() {
        for id in [
            "root/../etc",
            "root/./x",
            "/etc",
            "C:/x",
            "\\\\server\\share",
            "root/a\\b",
            "root/a:b",
            "root/.hidden",
            "root/.klt-tmp-1",
            "root/CON",
            "root/nul.txt",
            "root/trailing.",
            "root/trailing ",
        ] {
            assert!(VirtualPath::parse(id).is_err(), "{id} 应被拒绝");
        }
    }

    /// 长度与层级上限在解析阶段生效，避免极深或超长名字放大后续成本。
    #[test]
    fn enforces_limits() {
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(VirtualPath::parse(&format!("root/{long}")).is_err());
        let deep = vec!["a".to_owned(); MAX_PATH_COMPONENTS + 1];
        assert!(VirtualPath::from_segments(deep).is_err());
    }
}
