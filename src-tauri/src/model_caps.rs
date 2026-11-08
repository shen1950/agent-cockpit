//! The same key-scoped model facts drive discovery, client configuration and forwarding.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "model", rename_all = "snake_case")]
pub enum ImagePolicy {
    Native,
    Text,
    Describe(String),
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImagePolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<u32>,
}

const REASONING_LEVELS: [&str; 4] = ["off", "low", "medium", "high"];

pub type Policies = BTreeMap<String, Policy>;

fn path() -> std::path::PathBuf {
    crate::registry::base_dir().join("gateway-model-settings.json")
}

pub fn load() -> Result<Policies, String> {
    crate::client_connect::read(&path())?
        .map(|s| serde_json::from_str(&s).map_err(|_| "网关模型设置文件格式无效".to_owned()))
        .unwrap_or_else(|| Ok(Policies::new()))
}

/// Called under cmd_guard after the gateway has checked the selected key's catalog.
pub fn save(model: &str, provider: bool, policy: Policy, catalog: &[Value]) -> Result<(), String> {
    let row = catalog.iter().find(|m| m["id"] == model).ok_or("这把密钥不提供所选模型")?;
    let owner = row["owned_by"].as_str().ok_or("模型缺少来源")?;
    if provider && policy.image.is_some() {
        return Err("请为具体模型选择图片输入方式".into());
    }
    if policy.image == Some(ImagePolicy::Native) && !native_transport(model) {
        return Err("这条上游通道只接收文本，请选择视觉模型代读".into());
    }
    if let Some(ImagePolicy::Describe(id)) = &policy.image {
        if id == model || !catalog.iter().any(|m| m["id"] == *id && m["native_vision"] == true) {
            return Err("请选择这把密钥下另一个支持原生图片输入的模型".into());
        }
    }
    if [policy.context_window, policy.max_output_tokens, policy.auto_compact_token_limit]
        .contains(&Some(0)) {
        return Err("Token 数必须为正整数，留空可恢复自动".into());
    }
    if let Some(level) = &policy.reasoning {
        if !REASONING_LEVELS.contains(&level.as_str()) {
            return Err("推理思考只支持 关闭 / 低 / 中 / 高，留空恢复自动".into());
        }
    }
    if matches!((policy.context_window, policy.max_output_tokens), (Some(c), Some(o)) if o > c) {
        return Err("最大输出不能超过上下文窗口".into());
    }
    let scope = if provider { owner } else { model };
    let mut policies = load()?;
    if policy == Policy::default() {
        policies.remove(scope);
    } else {
        policies.insert(scope.to_owned(), policy);
    }
    crate::client_connect::write(&path(), &serde_json::to_string_pretty(&policies).map_err(|e| e.to_string())?)
}

fn native_transport(id: &str) -> bool {
    // These bridges flatten input to a text prompt, independent of vendor capabilities.
    !matches!(id.split_once('/').map(|(owner, _)| owner), Some("catpaw" | "opencode" | "qwenwork"))
}

pub fn positive(row: &Value, field: &str) -> Option<u64> {
    row[field].as_u64().filter(|n| *n > 0)
}

pub fn input(row: &Value) -> Value {
    if row["vision"] == true { json!(["text", "image"]) } else { json!(["text"]) }
}

/// Resolve defaults per field. A describer must be a native model in this very list:
/// two keys serving the same text model may legitimately advertise different modalities.
pub fn apply(rows: &mut [Value], settings: &Policies) {
    let empty = Policy::default();
    for row in rows.iter_mut() {
        let id = row["id"].as_str().unwrap_or_default();
        let model = settings.get(id).unwrap_or(&empty);
        let source = settings.get(row["owned_by"].as_str().unwrap_or_default()).unwrap_or(&empty);
        let official = crate::gateway_cost::facts(id);
        let public = official.as_ref().unwrap_or(&Value::Null);
        let can_forward = native_transport(id);
        let native = if !can_forward { Some(false) } else {
            match &model.image {
                Some(ImagePolicy::Native) => Some(true),
                Some(_) => Some(false),
                None => row["vision"].as_bool().or_else(|| public["vision"].as_bool()),
            }
        };
        row["native_vision"] = json!(native);
        row["native_image_transport"] = json!(can_forward);
        for (field, value) in [
            ("context_window", model.context_window.or(source.context_window)),
            ("max_output_tokens", model.max_output_tokens.or(source.max_output_tokens)),
            ("auto_compact_token_limit", model.auto_compact_token_limit.or(source.auto_compact_token_limit)),
        ] {
            if let Some(n) = value { row[field] = json!(n); }
            else if positive(row,field).is_none() {
                if let Some(n) = positive(public,field) { row[field] = json!(n); }
            }
        }
        let reasoning = model.reasoning.as_ref().or(source.reasoning.as_ref());
        row["reasoning_effort"] = json!(reasoning);
        if let Some(ctx) = positive(row, "context_window") {
            for field in ["auto_compact_token_limit", "max_output_tokens"] {
                if positive(row, field).is_some_and(|n| n > ctx) { row[field] = json!(ctx); }
            }
        }
    }
    let native: Vec<String> = rows.iter().filter(|r| r["native_vision"] == true)
        .filter_map(|r| r["id"].as_str().map(str::to_owned)).collect();
    for row in rows {
        let id = row["id"].as_str().unwrap_or_default();
        let policy = settings.get(id).and_then(|p| p.image.as_ref());
        let describer = match policy {
            Some(ImagePolicy::Describe(pick)) => native.iter().find(|n| *n == pick),
            None if row["native_vision"] != true => native.first(),
            _ => None,
        };
        row["vision_model"] = json!(describer);
        row["vision"] = json!(row["native_vision"] == true || describer.is_some());
        row["input_modalities"] = input(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_authorized_native_models_can_describe_and_limits_inherit_per_field() {
        let mut settings = Policies::new();
        settings.insert("trae".into(), Policy { context_window: Some(1_000_000), max_output_tokens: Some(64_000), ..Default::default() });
        settings.insert("trae/text".into(), Policy { context_window: Some(256_000), auto_compact_token_limit: Some(500_000),
            image: Some(ImagePolicy::Describe("other/eye".into())), reasoning: Some("high".into()), ..Default::default() });
        let raw = json!({"id":"trae/text","owned_by":"trae","vision":false});
        let mut rows = vec![raw.clone(), json!({"id":"other/eye","vision":true}), json!({"id":"catpaw/eye","vision":true})];
        apply(&mut rows, &settings);
        assert_eq!(rows[0]["max_output_tokens"], 64_000);
        assert_eq!(rows[0]["auto_compact_token_limit"], 256_000);
        assert_eq!(rows[0]["reasoning_effort"], "high");
        assert_eq!(rows[0]["input_modalities"], json!(["text","image"]));
        assert_eq!(rows[0]["vision_model"], "other/eye");
        assert_eq!(rows[2]["native_vision"], false);
        let mut restricted = [raw];
        apply(&mut restricted, &settings);
        assert_eq!(restricted[0]["input_modalities"], json!(["text"]));
        assert!(restricted[0]["vision_model"].is_null());
        assert_eq!(restricted[0]["reasoning_effort"], "high");
    }
}
