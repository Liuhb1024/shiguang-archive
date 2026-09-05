//! Identify source records without exposing raw or signed URLs to the WebView.
use serde_json::Value;

pub(crate) fn original_blog_url(raw: &Value) -> Option<String> {
    if raw.pointer("/cell_comm/appid")?.as_u64()? != 2 {
        return None;
    }
    let author = raw.pointer("/cell_userinfo/user/uin")?.as_str()?;
    let id = raw.pointer("/cell_id/cellid")?.as_str()?;
    if !(5..=12).contains(&author.len())
        || !(1..=20).contains(&id.len())
        || !author.bytes().all(|b| b.is_ascii_digit())
        || !id.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let expected = format!("/{author}/blog/{id}");
    for key in ["/cell_comm/curlikekey", "/cell_comm/orglikekey"] {
        let Some(input) = raw.pointer(key).and_then(Value::as_str) else {
            continue;
        };
        let Ok(url) = url::Url::parse(input) else {
            continue;
        };
        if matches!(url.scheme(), "https" | "http")
            && url.host_str() == Some("user.qzone.qq.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == expected
        {
            return Some(format!("https://user.qzone.qq.com{expected}"));
        }
    }
    None
}

pub(crate) fn provenance_note(content: Option<&str>, recovered: bool) -> &'static str {
    if recovered {
        "正文来自同条动态的更长历史通知，未核验为最新版本或完整全文。"
    } else if content.is_some_and(|s| s.trim_end().ends_with("...") || s.trim_end().ends_with('…'))
    {
        "当前仅保存疑似截断摘要，未获取可核验的完整全文。"
    } else {
        "正文来自交互记录，完整性未核验。"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(url: &str) -> Value {
        json!({"cell_comm":{"appid":2,"curlikekey":url},
          "cell_id":{"cellid":"1234567890"},
          "cell_userinfo":{"user":{"uin":"10001"}}})
    }

    #[test]
    fn blog_link_requires_source_identity_and_is_rebuilt_as_https() {
        assert_eq!(
            original_blog_url(&sample("http://user.qzone.qq.com/10001/blog/1234567890")),
            Some("https://user.qzone.qq.com/10001/blog/1234567890".into())
        );
        let mut other = sample("https://user.qzone.qq.com/10001/blog/1234567890");
        other["cell_comm"]["appid"] = json!(311);
        assert!(original_blog_url(&other).is_none());
    }

    #[test]
    fn blog_link_rejects_wrong_identity_secrets_and_untrusted_destinations() {
        for url in [
            "https://user.qzone.qq.com/10002/blog/1234567890",
            "https://user.qzone.qq.com/10001/blog/9999999999",
            "https://user.qzone.qq.com.evil.test/10001/blog/1234567890",
            "https://secret@user.qzone.qq.com/10001/blog/1234567890",
            "https://user.qzone.qq.com/10001/blog/1234567890?secret=fixture",
            "https://user.qzone.qq.com/10001/blog/1234567890#secret",
            "https://user.qzone.qq.com:444/10001/blog/1234567890",
            "javascript:alert(1)",
        ] {
            assert!(original_blog_url(&sample(url)).is_none());
        }
    }

    #[test]
    fn historical_and_truncated_exports_do_not_claim_full_text() {
        assert!(provenance_note(Some("摘要..."), false).contains("摘要"));
        assert!(provenance_note(Some("更长的历史文字"), true).contains("历史"));
        assert!(provenance_note(Some("没有省略号"), false).contains("未核验"));
    }
}
