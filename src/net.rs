//! 局域网监听地址枚举。
//! @author kongweiguang
//!
//! 只提供本机可用地址供用户选择，不猜测「哪个最像局域网」。排序把 RFC1918 私网地址
//! 排在前面、回环放最后，是因为插件的主要用途是同一局域网互传；但最终选择权在用户，
//! 服务端不会静默切换到别的地址。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 一个可绑定的本机地址。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenAddress {
    pub ip: IpAddr,
    /// 展示用名称（接口名或回环标记），不参与绑定。
    pub label: String,
}

/// 只把真实接口地址放进候选，再补回环兜底，避免系统返回的未指定地址成为默认值。
pub fn list_addresses() -> Vec<ListenAddress> {
    let mut result = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            let ip = interface.ip();
            if ip.is_loopback()
                || ip.is_unspecified()
                || result
                    .iter()
                    .any(|existing: &ListenAddress| existing.ip == ip)
            {
                continue;
            }
            result.push(ListenAddress {
                ip,
                label: interface.name.clone(),
            });
        }
    }
    // 私网地址优先：用户的典型场景是同一局域网，公网/链路本地地址排在后面。
    result.sort_by_key(|address| rank(address.ip));
    result.push(ListenAddress {
        ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        label: "loopback".to_owned(),
    });
    result
}

/// 排序权重：私网 IPv4 最前，其次其他 IPv4，再 IPv6，回环最后。
fn rank(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) if v4.is_private() => 0,
        IpAddr::V4(v4) if v4.is_link_local() => 3,
        IpAddr::V4(_) => 1,
        IpAddr::V6(v6) if v6.is_loopback() => 5,
        IpAddr::V6(v6) if is_unique_local(&v6) => 2,
        IpAddr::V6(v6) if v6.is_unicast_link_local() => 4,
        IpAddr::V6(_) => 4,
    }
}

/// IPv6 唯一本地地址（fc00::/7）与私网 IPv4 语义相同，排在普通地址之前。
fn is_unique_local(address: &Ipv6Addr) -> bool {
    address.octets()[0] & 0xfe == 0xfc
}

/// 通配监听时对外展示的具体 IPv4。
///
/// 链接与二维码必须给对端一个真实可打开的主机地址，`0.0.0.0` 只是绑定语义。
/// 这里沿用 `list_addresses` 的排序（私网优先），取第一个非回环 IPv4；没有
/// 局域网 IPv4 时回退回环，让面板能明确提示「仅本机可访问」，而不是把通配
/// 地址写进链接。
pub fn advertised_ipv4(addresses: &[ListenAddress]) -> Ipv4Addr {
    addresses
        .iter()
        .find_map(|address| match address.ip {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
            _ => None,
        })
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

/// 通配监听的 Host 白名单：启动时冻结本机真实 IPv4。
///
/// 冻结而不是在每个请求里重新枚举网卡，是为了让「启动时看到的地址」与
/// 「Host 校验接受的地址」保持一致，也避免运行期网卡变化引入不可预测的
/// 放行。回环显式加入：`list_addresses` 为排序把它放在末尾，但它仍是真实
/// 可访问的本地 IPv4；`0.0.0.0` 不是可访问主机，排除。
pub fn ipv4_allowlist(addresses: &[ListenAddress]) -> Vec<Ipv4Addr> {
    let mut allowed = Vec::new();
    for address in addresses {
        if let IpAddr::V4(ip) = address.ip
            && !ip.is_unspecified()
            && !allowed.contains(&ip)
        {
            allowed.push(ip);
        }
    }
    if !allowed.contains(&Ipv4Addr::LOCALHOST) {
        allowed.push(Ipv4Addr::LOCALHOST);
    }
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造地址项；测试不依赖真实网卡，直接用固定地址固定排序与筛选语义。
    fn listen(ip: &str, label: &str) -> ListenAddress {
        ListenAddress {
            ip: ip.parse().unwrap(),
            label: label.to_owned(),
        }
    }

    /// 对外地址优先选排序后的非回环 IPv4；只有回环时回退 127.0.0.1。
    #[test]
    fn advertised_ipv4_prefers_lan_and_falls_back_to_loopback() {
        let addresses = vec![
            listen("fd00::1", "eth0"),
            listen("127.0.0.1", "loopback"),
            listen("10.0.0.2", "eth0"),
        ];
        assert_eq!(advertised_ipv4(&addresses), Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(
            advertised_ipv4(&[listen("127.0.0.1", "loopback")]),
            Ipv4Addr::LOCALHOST
        );
        assert_eq!(advertised_ipv4(&[]), Ipv4Addr::LOCALHOST);
    }

    /// 白名单只包含真实 IPv4，始终补上回环，且不把通配地址当成可访问主机。
    #[test]
    fn ipv4_allowlist_freezes_real_addresses_and_loopback() {
        let addresses = vec![
            listen("0.0.0.0", "wildcard"),
            listen("192.168.1.10", "WLAN"),
            listen("fd00::1", "eth0"),
        ];
        let allowed = ipv4_allowlist(&addresses);
        assert_eq!(
            allowed,
            vec![Ipv4Addr::new(192, 168, 1, 10), Ipv4Addr::LOCALHOST]
        );
        assert!(!allowed.contains(&Ipv4Addr::UNSPECIFIED));
    }
}
