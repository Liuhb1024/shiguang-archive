use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use reqwest::{redirect::Policy, Client};
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointClass {
    Login,
    QzoneApi,
    Media,
}

fn host_matches(host: &str, expected: &str) -> bool {
    host == expected || host.ends_with(&format!(".{expected}"))
}

fn host_allowed(host: &str, class: EndpointClass) -> bool {
    match class {
        EndpointClass::Login => matches!(
            host,
            "xui.ptlogin2.qq.com" | "ssl.ptlogin2.qq.com" | "ptlogin2.qzone.qq.com"
        ),
        EndpointClass::QzoneApi => matches!(host, "h5.qzone.qq.com" | "mobile.qzone.qq.com"),
        EndpointClass::Media => {
            host == "photovideo.photo.qq.com"
                || host_matches(host, "photo.store.qq.com")
                || host_matches(host, "qpic.cn")
        }
    }
}

pub fn validate_outbound_url(input: &str, class: EndpointClass) -> Result<Url, String> {
    let url = Url::parse(input).map_err(|_| "网络地址格式无效".to_owned())?;
    if url.scheme() != "https" {
        return Err("仅允许 HTTPS 网络地址".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("网络地址不得包含用户凭据".into());
    }
    if url.port().is_some_and(|port| port != 443) {
        return Err("网络地址只能使用 HTTPS 默认端口".into());
    }
    let host = url
        .domain()
        .map(str::to_ascii_lowercase)
        .ok_or("网络地址必须使用允许的域名")?;
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return Err("不允许访问本机或局域网域名".into());
    }
    if !host_allowed(&host, class) {
        return Err("资源地址不在允许的腾讯域名范围内".into());
    }
    Ok(url)
}

pub fn validate_redirect(
    current: &Url,
    location: &str,
    class: EndpointClass,
) -> Result<Url, String> {
    let target = current
        .join(location)
        .map_err(|_| "服务器返回了无效的跳转地址".to_owned())?;
    validate_outbound_url(target.as_str(), class)
}

pub fn checked_response_size(
    current: usize,
    chunk: usize,
    maximum: usize,
) -> Result<usize, String> {
    let total = current
        .checked_add(chunk)
        .ok_or_else(|| "响应大小计算溢出".to_owned())?;
    if total > maximum {
        return Err(format!("响应超过 {} MB 安全限制", maximum / 1024 / 1024));
    }
    Ok(total)
}

pub async fn pinned_client(
    input: &str,
    class: EndpointClass,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<(Url, Client), String> {
    let url = validate_outbound_url(input, class)?;
    let host = url
        .domain()
        .map(str::to_owned)
        .ok_or("网络地址必须使用允许的域名")?;
    let port = url.port_or_known_default().unwrap_or(443);
    let mut addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| "无法解析允许的腾讯域名".to_owned())?
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        return Err("允许的腾讯域名没有可用地址".into());
    }
    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err("域名解析到了本机、局域网或保留地址，已阻止请求".into());
    }
    let client = Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .resolve_to_addrs(&host, &addresses)
        .build()
        .map_err(|_| "无法创建安全网络客户端".to_owned())?;
    Ok((url, client))
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            let [a, b, c, _] = address.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4() {
                return is_public_ip(IpAddr::V4(mapped));
            }
            let segments = address.segments();
            !(address.is_unspecified()
                || address.is_loopback()
                || address.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] & 0xffc0) == 0xfec0
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] & 0xfff0) == 0x3ff0
                || segments[0] == 0x2002)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{
        checked_response_size, is_public_ip, validate_outbound_url, validate_redirect,
        EndpointClass,
    };

    #[test]
    fn accepts_only_the_expected_tencent_hosts_for_each_class() {
        let allowed = [
            (
                "https://xui.ptlogin2.qq.com/cgi-bin/xlogin",
                EndpointClass::Login,
            ),
            ("https://ssl.ptlogin2.qq.com/ptqrshow", EndpointClass::Login),
            (
                "https://ptlogin2.qzone.qq.com/check_sig",
                EndpointClass::Login,
            ),
            (
                "https://h5.qzone.qq.com/mqzone/index",
                EndpointClass::QzoneApi,
            ),
            (
                "https://mobile.qzone.qq.com/get_feeds",
                EndpointClass::QzoneApi,
            ),
            (
                "https://photovideo.photo.qq.com/a.mp4",
                EndpointClass::Media,
            ),
            ("https://a1.photo.store.qq.com/a.jpg", EndpointClass::Media),
            ("https://m.qpic.cn/a.jpg", EndpointClass::Media),
        ];

        for (url, class) in allowed {
            assert!(validate_outbound_url(url, class).is_ok(), "应允许 {url}");
        }
    }

    #[test]
    fn rejects_disguised_hosts_and_unsafe_url_features() {
        let rejected = [
            (
                "http://mobile.qzone.qq.com/get_feeds",
                EndpointClass::QzoneApi,
            ),
            ("https://evilqq.com/a.jpg", EndpointClass::Media),
            ("https://qpic.cn.evil.test/a.jpg", EndpointClass::Media),
            (
                "https://mobile.qzone.qq.com.evil.test/get_feeds",
                EndpointClass::QzoneApi,
            ),
            ("https://127.0.0.1/a.jpg", EndpointClass::Media),
            ("https://localhost/a.jpg", EndpointClass::Media),
            (
                "https://user:secret@mobile.qzone.qq.com/get_feeds",
                EndpointClass::QzoneApi,
            ),
            (
                "https://mobile.qzone.qq.com:444/get_feeds",
                EndpointClass::QzoneApi,
            ),
            ("https://h5.qzone.qq.com/a.jpg", EndpointClass::Media),
            ("https://user.qzone.qq.com/123", EndpointClass::QzoneApi),
        ];

        for (url, class) in rejected {
            assert!(validate_outbound_url(url, class).is_err(), "应拒绝 {url}");
        }
    }

    #[test]
    fn rejects_non_public_ip_ranges() {
        let rejected = [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
        ];
        for value in rejected {
            let ip: IpAddr = value.parse().unwrap();
            assert!(!is_public_ip(ip), "应拒绝 {value}");
        }

        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn redirects_are_revalidated_against_the_same_allowlist() {
        let current =
            validate_outbound_url("https://m.qpic.cn/original.jpg", EndpointClass::Media).unwrap();
        assert_eq!(
            validate_redirect(&current, "/final.jpg", EndpointClass::Media)
                .unwrap()
                .as_str(),
            "https://m.qpic.cn/final.jpg"
        );
        assert!(
            validate_redirect(&current, "https://evil.example/steal", EndpointClass::Media)
                .is_err()
        );
        assert!(
            validate_redirect(&current, "http://m.qpic.cn/downgrade", EndpointClass::Media)
                .is_err()
        );
    }

    #[test]
    fn response_size_is_checked_incrementally() {
        assert_eq!(checked_response_size(8, 2, 10).unwrap(), 10);
        assert!(checked_response_size(8, 3, 10).is_err());
        assert!(checked_response_size(usize::MAX, 1, usize::MAX).is_err());
    }
}
