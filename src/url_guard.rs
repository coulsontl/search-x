use std::net::IpAddr;

/// 只允许公网 http/https URL，拒绝私网、回环、链路本地、CGNAT、保留地址与内网域名。
pub fn is_public_http_url(raw: &str) -> bool {
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa")
    {
        return false;
    }
    // IPv6 字面量在 URL 里带方括号（如 [::1]），解析 IP 前去掉。
    let ip_candidate = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip_candidate.parse::<IpAddr>() {
        return is_public_ip(ip);
    }
    true
}

/// 在字符串检查之后再做一次 DNS 解析：所有解析结果都必须是公网 IP。
/// 用于 /fetch 把 URL 交给第三方抓取前，阻止 `127.0.0.1.nip.io` 这类解析到内网的域名。
pub async fn is_public_http_url_resolved(raw: &str) -> bool {
    if !is_public_http_url(raw) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(raw) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let resolved = tokio::net::lookup_host((host.as_str(), 80)).await;
    match resolved {
        Ok(addresses) => {
            let mut resolved_any = false;
            for address in addresses {
                resolved_any = true;
                if !is_public_ip(address.ip()) {
                    return false;
                }
            }
            resolved_any
        }
        Err(_) => false,
    }
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => {
            // ::ffff:a.b.c.d（IPv4-mapped）必须按对应 IPv4 判断，
            // 否则 [::ffff:127.0.0.1] 会绕过回环/私网检查。
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ipv4(v4);
            }
            let segments = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (segments[0] & 0xffc0) == 0xfe80) // fe80::/10 link local
        }
    }
}

fn is_public_ipv4(v4: std::net::Ipv4Addr) -> bool {
    let octets = v4.octets();
    let cgnat = octets[0] == 100 && (64..128).contains(&octets[1]);
    !(v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_documentation()
        || v4.is_multicast()
        || cgnat)
}

#[cfg(test)]
mod tests {
    use super::is_public_http_url;

    #[test]
    fn blocks_internal_targets() {
        for url in [
            "http://localhost/admin",
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://192.168.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:10.0.0.1]/",
            "http://[fd00::1]/",
            "http://foo.internal/",
            "file:///etc/passwd",
            "https://user:pass@example.com/",
        ] {
            assert!(!is_public_http_url(url), "{url} should be blocked");
        }
    }

    #[test]
    fn allows_public_targets() {
        for url in [
            "https://example.com/",
            "https://www.ithome.com/1/030/332.htm",
            "http://1.1.1.1/",
            "http://[::ffff:8.8.8.8]/",
        ] {
            assert!(is_public_http_url(url), "{url} should be allowed");
        }
    }
}
