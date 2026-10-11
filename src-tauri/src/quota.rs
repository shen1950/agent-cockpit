// 额度：过期倒计时 + 自动抓取（OpenAI 官方适配器 / 通用 JSON 接口）
use chrono::{Local, NaiveDate, NaiveDateTime};
use serde_json::Value;

pub fn days_left(expires_on: &str) -> Option<i64> {
    let s = expires_on.trim();
    if s.is_empty() {
        return None;
    }
    // 到期时间从 agenthub 过来是精确到分的（"2026-10-11 15:31"），倒计时只看日期部分；
    // "未给出""无限期"这类文字截不出日期，自然返回 None（不冒充承诺）。
    let head = &s[..s.len().min(10)];
    let d = NaiveDate::parse_from_str(head, "%Y-%m-%d").ok()?;
    let today = Local::now().date_naive();
    Some(d.signed_duration_since(today).num_days())
}

/// (是否已过期, 剩余天数)。
///
/// 过期判定必须到分钟：WorkBuddy 的裂变包写着"今天 15:31 到期"，13:43 看它还得是有效期内，
/// 过了那一刻才该挪进「已过期」。只给到日的按当天 23:59 算（当天还能用）。
pub fn expiry(expires_on: &str) -> (bool, Option<i64>) {
    let s = expires_on.trim();
    let dl = days_left(s);
    if s.is_empty() {
        return (false, None);
    }
    let end = match NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        Ok(t) => Some(t),
        Err(_) => NaiveDate::parse_from_str(&s[..s.len().min(10)], "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(23, 59, 59)),
    };
    // "未给出" / "无限期" 截不出时间 → 不判过期，天数照 days_left 给 None
    match end {
        Some(t) => (t < Local::now().naive_local(), dl),
        None => (false, dl),
    }
}

/// 返回 (剩余, 总额, 错误)
pub fn fetch_quota(meta: &Value) -> (Option<f64>, Option<f64>, Option<String>) {
    let token = meta["token"].as_str().unwrap_or("").trim().to_string();
    let adapter = meta["quota_adapter"].as_str().unwrap_or("").trim().to_string();
    let base = meta["base_url"]
        .as_str()
        .unwrap_or("")
        .trim()
        .trim_end_matches('/')
        .to_string();
    let url = match adapter.as_str() {
        "openai" => "https://api.openai.com/dashboard/billing/credit_grants".to_string(),
        // one-api / new-api 系中转站：根地址（去掉 /v1）+ /api/user/self 返回 quota / used_quota
        "newapi" | "oneapi" => {
            let root = base.trim_end_matches("/v1").trim_end_matches('/');
            if root.is_empty() {
                return (None, None, Some("one-api/new-api 适配器需要 Base URL".into()));
            }
            format!("{}/api/user/self", root)
        }
        _ => meta["quota_url"].as_str().unwrap_or("").trim().to_string(),
    };
    if url.is_empty() {
        return (None, None, Some("未配置适配器或额度接口".into()));
    }
    if adapter == "openai" && token.is_empty() {
        return (None, None, Some("OpenAI 适配器需要 API Token".into()));
    }

    let mut req = ureq::get(&url).timeout(std::time::Duration::from_secs(15));
    req = req.set("User-Agent", "agent-cockpit/2.0");
    if !token.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", token));
    }
    if let Ok(extra) = serde_json::from_str::<Value>(meta["quota_headers"].as_str().unwrap_or("{}")) {
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                req = req.set(k, v.as_str().unwrap_or(""));
            }
        }
    }

    let body: Value = match req.call() {
        Ok(r) => match r.into_json() {
            Ok(v) => v,
            Err(e) => return (None, None, Some(format!("响应不是合法 JSON：{}", e))),
        },
        Err(ureq::Error::Status(code, _)) => {
            return (None, None, Some(format!("抓取失败：HTTP {}", code)))
        }
        Err(e) => return (None, None, Some(format!("抓取失败：{}", e))),
    };

    if adapter == "openai" {
        let total = body["total_granted"].as_f64();
        let used = body["total_used"].as_f64().unwrap_or(0.0);
        return match total {
            Some(t) => (Some(t - used), Some(t), None),
            None => (None, None, Some("响应缺少 total_granted（key 可能无账单权限）".into())),
        };
    }
    if adapter == "newapi" || adapter == "oneapi" {
        let data = &body["data"];
        let quota = data["quota"].as_f64();
        let used = data["used_quota"].as_f64().unwrap_or(0.0);
        return match quota {
            Some(q) => (Some(q - used), Some(q), None),
            None => (None, None, Some("响应缺少 data.quota（该站不是 one-api/new-api 兼容）".into())),
        };
    }

    let mut node = body.clone();
    for key in meta["quota_path"].as_str().unwrap_or("").split('.') {
        if !key.is_empty() {
            node = match node.get(key) {
                Some(v) => v.clone(),
                None => Value::Null,
            };
        }
    }
    if !node.is_number() {
        for key in [
            "remaining",
            "remaining_credits",
            "credits_left",
            "total_available",
            "grant_remaining",
        ] {
            if let Some(v) = body.get(key) {
                if v.is_number() {
                    node = v.clone();
                    break;
                }
            }
        }
    }
    match node.as_f64() {
        Some(v) => (Some(v), None, None),
        None => (None, None, Some("响应中未找到数值字段，请设置取值路径".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn at(offset_days: i64, hh: u32, mm: u32) -> String {
        let t = Local::now().naive_local() + Duration::days(offset_days);
        t.date().and_hms_opt(hh, mm, 0).unwrap().format("%Y-%m-%d %H:%M").to_string()
    }

    /// 到期判定到分钟：今天但还没到点的那笔仍在有效期内
    #[test]
    fn minute_precision_expiry() {
        let now = Local::now().naive_local();
        let later = (now + Duration::hours(6)).format("%Y-%m-%d %H:%M").to_string();
        let earlier = (now - Duration::hours(6)).format("%Y-%m-%d %H:%M").to_string();
        assert!(!expiry(&later).0, "还没到点：{later}");
        assert!(expiry(&earlier).0, "已过点：{earlier}");
        // 天数按"跨没跨到次日"现算，不许写死 0：晚上 6 点以后 now+6h 已经是明天，
        // 写死 0 的断言每天到点就自己挂了
        let want = (now + Duration::hours(6)).date() - now.date();
        assert_eq!(expiry(&later).1, Some(want.num_days()));
    }

    /// 只给到日的按当天 23:59 算（今天到期的今天还能用）；"未给出""无限期"既不判过期也不给天数
    #[test]
    fn date_only_and_unknown_expiry() {
        let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
        assert!(!expiry(&today).0, "只写日期 = 当天 23:59 才作废：{today}");
        assert!(expiry("2020-01-01").0);
        for s in ["", "未给出", "无限期"] {
            assert!(!expiry(s).0, "{s} 不该判成过期");
            assert_eq!(expiry(s).1, None, "{s} 没有天数可报");
        }
    }

    /// days_left 认得带分钟的写法（agenthub 现在给的是 "YYYY-MM-DD HH:MM"）
    #[test]
    fn days_left_accepts_datetime() {
        assert_eq!(days_left(&at(9, 22, 18)), Some(9));
        assert_eq!(days_left("2026-10-20"), days_left("2026-10-20 07:29"));
    }
}
