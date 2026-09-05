use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, COOKIE, ORIGIN, PRAGMA, REFERER, USER_AGENT,
};
use serde_json::Value;

use crate::{
    network_policy::{checked_response_size, pinned_client, EndpointClass},
    qlogin::QLoginState,
};

const FEEDS_URL: &str = "https://mobile.qzone.qq.com/get_feeds";
const FEED_RESPONSE_ATTEMPTS: u32 = 3;
const MAX_FEED_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

fn retryable_response_reason(status: reqwest::StatusCode, body: &str) -> Option<String> {
    if status.is_server_error() {
        return Some(format!("HTTP {status}"));
    }
    if !status.is_success() {
        return None;
    }
    let value = match serde_json::from_str::<Value>(body) {
        Ok(value) => value,
        Err(_) => return Some("响应不是有效 JSON".into()),
    };
    if let Some(code) = value.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let message = value
                .get("message")
                .or_else(|| value.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            let permanent = [
                "未登录",
                "登录失效",
                "权限",
                "封禁",
                "禁止访问",
                "p_skey",
                "频繁",
                "频率",
                "验证码",
                "风控",
                "安全验证",
            ]
            .iter()
            .any(|keyword| message.contains(keyword));
            return (!permanent).then(|| format!("接口错误 {code}"));
        }
    }
    if value.get("data").is_none() {
        return Some("响应中暂时缺少 data".into());
    }
    None
}

fn feed_retry_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(1_500 * 2_u64.pow(attempt.saturating_sub(1)))
}

fn sec_ch_ua(user_agent: &str) -> String {
    if let Some(start) = user_agent.find("Chrome/") {
        let version_start = start + 7;
        let major = user_agent[version_start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        let version = if major.is_empty() { "131" } else { &major };
        format!("\"Not;A=Brand\";v=\"8\", \"Chromium\";v=\"{version}\", \"Microsoft Edge\";v=\"{version}\"")
    } else {
        "\"Not;A=Brand\";v=\"8\", \"Apple\";v=\"0\", \"Safari\";v=\"18\"".to_owned()
    }
}

fn sec_platform(user_agent: &str) -> &'static str {
    if user_agent.contains("iPhone") {
        "\"iOS\""
    } else {
        "\"Android\""
    }
}
fn log_feed_request_error(stage: &str, status: Option<reqwest::StatusCode>) {
    eprintln!(
        "[QzoneArchive] QQ 空间请求失败：stage={stage}, status={}",
        status
            .map(|value| value.as_u16().to_string())
            .unwrap_or_else(|| "none".to_owned())
    );
}

#[derive(Debug)]
pub struct FeedPage {
    pub(crate) feeds: Vec<Value>,
    pub(crate) attach_info: Option<String>,
    pub(crate) has_more: bool,
}

fn parse_feed_page(value: Value) -> Result<FeedPage, String> {
    if let Some(code) = value.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let message = value
                .get("message")
                .or_else(|| value.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            let reason = if ["频繁", "频率", "验证码", "风控", "安全验证"]
                .iter()
                .any(|s| message.contains(s))
            {
                "触发访问限制，请停止采集并稍后通过官方客户端确认"
            } else if ["未登录", "登录失效", "p_skey"]
                .iter()
                .any(|s| message.contains(s))
            {
                "登录失效，请重新扫码"
            } else {
                "接口拒绝了请求，请确认账号权限后再试"
            };
            return Err(format!("QQ_ACCOUNT_STOP:{code}：{reason}"));
        }
    }
    let data = value.get("data").ok_or("动态响应中缺少 data")?;
    let feeds = data
        .get("vFeeds")
        .and_then(Value::as_array)
        .cloned()
        .ok_or("动态响应缺少有效记录列表，已停止以免误报完成")?;
    let attach_info = data
        .get("attachinfo")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let server_has_more = data.get("hasmore").and_then(Value::as_i64).unwrap_or(0) != 0;
    if server_has_more && (feeds.is_empty() || attach_info.is_none()) {
        return Err("分页响应不完整，已停止并保留上次进度".into());
    }
    let has_more = server_has_more && !feeds.is_empty() && attach_info.is_some();
    Ok(FeedPage {
        feeds,
        attach_info,
        has_more,
    })
}

pub(crate) async fn fetch_feeds(
    state: &QLoginState,
    refresh_type: &str,
    attach_info: Option<&str>,
    before_attempt: impl FnMut() -> Result<(), String>,
) -> Result<FeedPage, String> {
    fetch_feeds_with_attempts(
        state,
        refresh_type,
        attach_info,
        FEED_RESPONSE_ATTEMPTS,
        before_attempt,
    )
    .await
}

pub(crate) async fn fetch_feeds_once(
    state: &QLoginState,
    refresh_type: &str,
    attach_info: Option<&str>,
    before_attempt: impl FnMut() -> Result<(), String>,
) -> Result<FeedPage, String> {
    fetch_feeds_with_attempts(state, refresh_type, attach_info, 1, before_attempt).await
}

pub(crate) fn feed_error_can_skip(error: &str) -> bool {
    error.contains("HTTP 5") || error.starts_with("解析空间动态失败：")
}

async fn fetch_feeds_with_attempts(
    state: &QLoginState,
    refresh_type: &str,
    attach_info: Option<&str>,
    attempts: u32,
    mut before_attempt: impl FnMut() -> Result<(), String>,
) -> Result<FeedPage, String> {
    let auth = state.qzone_auth().await?;
    let mut query = vec![
        ("g_tk", auth.g_tk.to_string()),
        ("res_type", "1".into()),
        ("refresh_type", refresh_type.into()),
        ("format", "json".into()),
    ];
    if let Some(attach_info) = attach_info {
        if attach_info.trim().is_empty() {
            let error = "分页游标不能为空";
            log_feed_request_error("validate_request", None);
            return Err(error.into());
        }
        query.push(("res_attach", attach_info.to_owned()));
    }
    let (feeds_url, client) = pinned_client(
        FEEDS_URL,
        EndpointClass::QzoneApi,
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(35),
    )
    .await?;
    let mut response = None;
    let mut last_error = None;
    let mut failed_response_status = None;
    let mut last_attempt_logged = false;
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        before_attempt()?;
        match client
            .get(feeds_url.clone())
            .header(ACCEPT, "application/json")
            .header(
                ACCEPT_LANGUAGE,
                "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6,zh-TW;q=0.5",
            )
            .header(CACHE_CONTROL, "no-cache")
            .header(PRAGMA, "no-cache")
            .header(ORIGIN, "https://h5.qzone.qq.com")
            .header(REFERER, "https://h5.qzone.qq.com/")
            .header(USER_AGENT, &auth.user_agent)
            .header(COOKIE, &auth.cookie_header)
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-site")
            .header("Sec-Ch-Ua", sec_ch_ua(&auth.user_agent))
            .header("Sec-Ch-Ua-Mobile", "?1")
            .header("Sec-Ch-Ua-Platform", sec_platform(&auth.user_agent))
            .query(&query)
            .send()
            .await
        {
            Ok(mut value) => {
                let status = value.status();
                let mut bytes = Vec::new();
                let mut read_error = None;
                loop {
                    match value.chunk().await {
                        Ok(Some(chunk)) => {
                            if let Err(reason) = checked_response_size(
                                bytes.len(),
                                chunk.len(),
                                MAX_FEED_RESPONSE_BYTES,
                            ) {
                                read_error = Some(reason);
                                break;
                            }
                            bytes.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(_) => {
                            read_error = Some("网络响应读取失败".to_owned());
                            break;
                        }
                    }
                }
                let body = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(reason) = read_error {
                    let detail = format!(
                        "响应体读取失败（第 {attempt}/{attempts} 次，已接收 {} 字节）：{reason}",
                        bytes.len()
                    );
                    last_error = Some(detail);
                    log_feed_request_error(
                        &format!("read_response_attempt_{attempt}"),
                        Some(status),
                    );
                    failed_response_status = Some(status);
                    last_attempt_logged = true;
                    if attempt < attempts {
                        tokio::time::sleep(feed_retry_delay(attempt)).await;
                    }
                } else {
                    if retryable_response_reason(status, &body).is_some() {
                        log_feed_request_error(
                            &format!("retryable_response_attempt_{attempt}"),
                            Some(status),
                        );
                        if attempt < attempts {
                            tokio::time::sleep(feed_retry_delay(attempt)).await;
                            continue;
                        }
                    }
                    response = Some((status, body));
                    break;
                }
            }
            Err(error) => {
                let kind = if error.is_timeout() {
                    "请求超时"
                } else if error.is_connect() {
                    "连接失败"
                } else {
                    "传输失败"
                };
                let detail = format!("{kind}（第 {attempt}/{attempts} 次）");
                last_error = Some(detail);
                last_attempt_logged = false;
                if attempt < attempts {
                    tokio::time::sleep(feed_retry_delay(attempt)).await;
                }
            }
        }
    }
    let Some((status, body)) = response else {
        let error = format!(
            "获取空间动态失败：{}",
            last_error.unwrap_or_else(|| "未知网络错误".into())
        );
        let stage = if failed_response_status.is_some() {
            "read_response"
        } else {
            "transport"
        };
        if !last_attempt_logged {
            log_feed_request_error(stage, failed_response_status);
        }
        return Err(error);
    };
    if !status.is_success() {
        let error = format!("获取空间动态失败：HTTP {status}");
        log_feed_request_error("http_status", Some(status));
        return Err(error);
    }
    let value = match serde_json::from_str::<Value>(&body) {
        Ok(value) => value,
        Err(reason) => {
            let error = format!("解析空间动态失败：{reason}");
            log_feed_request_error("parse_json", Some(status));
            return Err(error);
        }
    };
    match parse_feed_page(value) {
        Ok(page) => Ok(page),
        Err(error) => {
            log_feed_request_error("parse_api_response", Some(status));
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{feed_error_can_skip, parse_feed_page, retryable_response_reason, FEEDS_URL};
    use reqwest::StatusCode;
    use serde_json::json;

    #[test]
    fn keeps_first_page_feeds_and_cursor() {
        let page = parse_feed_page(json!({
            "code": 0,
            "data": { "attachinfo": "next-cursor", "hasmore": 1, "vFeeds": [{"id": 1}] }
        }))
        .unwrap();
        assert_eq!(page.feeds.len(), 1);
        assert_eq!(page.attach_info.as_deref(), Some("next-cursor"));
        assert!(page.has_more);
    }

    #[test]
    fn empty_page_finishes_pagination() {
        let page = parse_feed_page(json!({"code": 0, "data": {"vFeeds": []}})).unwrap();
        assert!(page.feeds.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn cursor_remains_server_encoded_until_query_serialization() {
        let cursor = "att=back%5Fserver%5Finfo%3Doffset%253D6&tl=123";
        let encoded =
            reqwest::Url::parse_with_params(FEEDS_URL, &[("res_attach", cursor)]).unwrap();
        assert!(encoded
            .as_str()
            .contains("back%255Fserver%255Finfo%253Doffset%25253D6%26tl%3D123"));
        assert_eq!(
            encoded
                .query_pairs()
                .find(|(key, _)| key == "res_attach")
                .unwrap()
                .1,
            cursor
        );
    }

    #[test]
    fn stops_rate_limits_but_retries_temporary_api_errors() {
        assert!(retryable_response_reason(StatusCode::TOO_MANY_REQUESTS, "busy").is_none());
        assert!(retryable_response_reason(
            StatusCode::OK,
            r#"{"code":-1,"message":"系统繁忙，请稍后再试"}"#,
        )
        .is_some());
    }

    #[test]
    fn does_not_retry_expired_login_response() {
        assert!(retryable_response_reason(
            StatusCode::OK,
            r#"{"code":-3000,"message":"登录失效，请重新登录"}"#,
        )
        .is_none());
    }

    #[test]
    fn only_skips_page_specific_server_or_response_errors() {
        assert!(feed_error_can_skip(
            "获取空间动态失败：HTTP 500 Internal Server Error"
        ));
        assert!(feed_error_can_skip("解析空间动态失败：expected value"));
        assert!(!feed_error_can_skip(
            "获取空间动态失败：HTTP 429 Too Many Requests"
        ));
        assert!(!feed_error_can_skip("尚未登录 QQ 空间"));
    }

    #[test]
    fn core_fix_never_skips_account_or_risk_errors() {
        for message in ["登录失效", "访问频繁，请稍后再试", "验证码", "禁止访问"]
        {
            let error = parse_feed_page(json!({"code": -1, "message": message})).unwrap_err();
            assert!(!feed_error_can_skip(&error), "{message}");
        }
    }

    #[test]
    fn core_fix_does_not_expose_untrusted_api_messages() {
        let error =
            parse_feed_page(json!({"code": -1, "message": "sensitive-fixture-url-and-cookie"}))
                .unwrap_err();
        assert!(!error.contains("sensitive-fixture"));
    }

    #[test]
    fn core_fix_missing_feeds_or_cursor_is_not_reported_as_completed() {
        assert!(parse_feed_page(json!({"code":0,"data":{}})).is_err());
        assert!(
            parse_feed_page(json!({"code":0,"data":{"hasmore":1,"vFeeds":[{"id":1}]}})).is_err()
        );
        assert!(parse_feed_page(
            json!({"code":0,"data":{"hasmore":1,"attachinfo":"cursor","vFeeds":[]}})
        )
        .is_err());
    }
}
