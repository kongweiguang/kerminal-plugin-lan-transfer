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

/// 枚举本机非回环地址，再补上回环作为兜底；同一 IP 只出现一次。
pub fn list_addresses() -> Vec<ListenAddress> {
    let mut result = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            let ip = interface.ip();
            if ip.is_loopback()
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
