//! Codex 客户端的模型目录（model catalog）生成。
//!
//! Codex 桌面端/CLI 的模型选择器默认显示**内置的 OpenAI 推荐集**（gpt-6.1-sol
//! 那一排），用户完全看不出背后跑的是什么。实测（codex-cli 0.163.0）发现
//! `[model_providers.X]` 支持 `model_catalog_url`：客户端会带着该账号的
//! `Authorization: Bearer` 去 GET 这个地址，解析成功后**选择器与默认模型都改用
//! 目录内容**（slug 即请求里的 model 字段，display_name 即选择器显示名）。
//!
//! 目录条目字段极多且有几处反直觉的必填（实测定位，2026-10-31）：
//! - `model_messages` 必填（整对象不能少），`base_instructions` 可为 null（去了
//!   就走 Codex 内置提示词，正好别把 GPT-5 的提示词发给第三方模型）；
//! - `use_responses_lite` 是必填 bool，不能 null；`comp_hash`/`tool_mode` 可 null；
//! - `prefer_websockets` 必须给 false —— 本反代没有 WebSocket 传输；
//! - 解析失败客户端会静默回落到内置推荐集，所以宁可字段照抄模板也别缺。
//!
//! 模板条目取自真实下发目录（`codex_model_template.json`，GPT 系模型的一条完整
//! 记录），构建时逐模型覆写身份/传输/推理字段。
use serde_json::{json, Value};
use std::sync::OnceLock;

const TEMPLATE_JSON: &str = include_str!("codex_model_template.json");

/// 目录里一套最朴素的推理档位（选择器里的「轻度/中度/高度」后缀来自这里）。
/// 上游各家对 effort 的支持不一，只给保守的三档，默认 medium。
fn reasoning_levels() -> Value {
    json!([
        { "effort": "low", "description": "轻量推理，响应最快" },
        { "effort": "medium", "description": "速度与推理深度均衡" },
        { "effort": "high", "description": "复杂问题深度推理" },
    ])
}

fn template() -> &'static Value {
    static T: OnceLock<Value> = OnceLock::new();
    T.get_or_init(|| {
        serde_json::from_str(TEMPLATE_JSON)
            .unwrap_or_else(|_| json!({ "slug": "", "display_name": "", "description": "" }))
    })
}

fn supports_images(model: &Value) -> bool {
    if let Some(vision) = model["vision"].as_bool() {
        return vision;
    }
    // The generic model listing may omit capability metadata. Resolve MiniMax
    // from its authoritative definitions instead of duplicating a model whitelist.
    model["id"].as_str()
        .and_then(|id| id.strip_prefix("minimax/"))
        .and_then(|id| crate::mmproxy::models().into_iter()
            .find(|m| m["id"].as_str() == Some(id)))
        .and_then(|m| m["vision"].as_bool())
        .unwrap_or(false)
}

/// 把反代的模型清单（`models_for` 的产出，id 形如 `minimax/MiniMax-M3.1-Flash`）
/// 翻成 codex 目录格式。slug 保留 `agent/模型` 全名 —— 请求发回来时前缀路由
/// 正好接着用。
pub fn catalog(models: &[Value]) -> Value {
    let tpl = template();
    let list: Vec<Value> = models
        .iter()
        .filter(|m| !m["id"].as_str().unwrap_or("").starts_with("catpaw/"))
        .enumerate()
        .map(|(i, m)| {
            let id = m["id"].as_str().unwrap_or("");
            let name = m["name"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(id);
            // 上下文窗口：models_for 里有的厂商带（minimax 等），没有就给个保守默认
            let ctx = crate::model_caps::positive(m, "context_window").unwrap_or(200_000);
            let mut e = tpl.clone();
            let obj = e.as_object_mut().expect("目录模板必须是对象");
            obj.insert("slug".into(), json!(id));
            // 显示名 = 带前缀的完整 id（选择器实测正常渲染）：各家模型常有同名，
            // 裸名在选择器里分不清是哪家的。友好名放 description 兜底。
            obj.insert("display_name".into(), json!(id));
            obj.insert("description".into(), json!(format!("{id} · {name} · AgentCockpit 反代")));
            obj.insert("visibility".into(), json!("list"));
            obj.insert("supported_in_api".into(), json!(true));
            obj.insert("priority".into(), json!(i as u64));
            obj.insert("context_window".into(), json!(ctx));
            obj.insert("max_context_window".into(), json!(ctx));
            // 自动压缩阈值跟着上下文走，别用模板里 GPT 的 244800
            let compact = crate::model_caps::positive(m, "auto_compact_token_limit")
                .unwrap_or_else(|| ctx.saturating_mul(8) / 10).min(ctx);
            obj.insert("auto_compact_token_limit".into(), json!(compact));
            // Only advertise image input when the upstream explicitly supports it.
            // Codex rejects image attachments locally when this capability is absent.
            obj.insert("input_modalities".into(), if supports_images(m) {
                json!(["text", "image"])
            } else {
                json!(["text"])
            });
            obj.insert(
                "supported_reasoning_levels".into(),
                m["supported_reasoning_levels"].as_array().filter(|levels| !levels.is_empty())
                    .map(|levels| json!(levels)).unwrap_or_else(reasoning_levels),
            );
            obj.insert("default_reasoning_level".into(), json!(m["default_reasoning_level"].as_str().unwrap_or("medium")));
            // Custom grammar tools are unsupported by these gateways. Codex keeps
            // its standard terminal tools for file editing.
            obj.insert("apply_patch_tool_type".into(), Value::Null);
            obj.insert("default_reasoning_summary".into(), json!("none"));
            // 传输相关：本反代只有 HTTP + SSE，关掉一切花活
            obj.insert("prefer_websockets".into(), json!(false));
            obj.insert("use_responses_lite".into(), json!(false));
            obj.insert("tool_mode".into(), Value::Null);
            // 提示词交还 Codex 内置默认，别把 GPT-5 的模板发给第三方模型
            obj.insert("base_instructions".into(), Value::Null);
            e
        })
        .collect();
    json!({ "models": list })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_builds_entries_from_model_list() {
        let models = vec![
            json!({ "id": "minimax/MiniMax-M3.1-Flash", "name": "MiniMax M3.1 Flash",
                    "context_window": 200000, "object": "model" }),
            json!({ "id": "catpaw/MiniMax-M3.1-Flash", "name": "MiniMax M3.1 Flash", "object": "model" }),
            json!({ "id": "trae/glm-5.3-flash", "name": "GLM 5.3 Flash", "object": "model" }),
        ];
        let cat = catalog(&models);
        let list = cat["models"].as_array().unwrap();
        // catpaw 被剔除：妙手云端拒客户端工具调用，Codex 里永远用不通
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|m| !m["slug"].as_str().unwrap().starts_with("catpaw/")));
        // slug 与显示名都带 agent/ 前缀：同名模型分得清是哪家的
        assert_eq!(list[0]["slug"], "minimax/MiniMax-M3.1-Flash");
        assert_eq!(list[0]["display_name"], "minimax/MiniMax-M3.1-Flash");
        assert_eq!(list[1]["display_name"], "trae/glm-5.3-flash");
        assert!(list[0]["description"].as_str().unwrap().contains("MiniMax M3.1 Flash"), "友好名留在了 description");
        // 身份/传输/推理字段按实测约束覆写
        assert!(list[0]["base_instructions"].is_null(), "GPT 提示词不能带过去");
        assert_eq!(list[0]["prefer_websockets"], false);
        assert_eq!(list[0]["use_responses_lite"], false);
        assert!(list[0]["tool_mode"].is_null());
        assert_eq!(list[0]["visibility"], "list");
        assert_eq!(list[0]["priority"], 0);
        assert_eq!(list[0]["context_window"], 200000);
        assert_eq!(list[0]["auto_compact_token_limit"], 160000);
        // 必填字段一个不能少（实测 model_messages 缺了整个目录都解析不了）
        assert!(list[0]["model_messages"].is_object());
        assert!(list[0]["supported_reasoning_levels"].as_array().unwrap().len() >= 3);
        assert_eq!(list[0]["default_reasoning_level"], "medium");
    }

    #[test]
    fn catalog_preserves_provider_reasoning_without_unsupported_custom_tools() {
        let catalog = catalog(&[json!({
            "id": "astudio/spark-x2.6-4b",
            "supported_reasoning_levels": [{"effort": "none", "description": "Off"}, {"effort": "high", "description": "On"}],
            "default_reasoning_level": "high"
        })]);
        let model = &catalog["models"][0];
        assert!(model["apply_patch_tool_type"].is_null());
        assert_eq!(model["default_reasoning_level"], "high");
        assert_eq!(model["supported_reasoning_levels"][0]["effort"], "none");
        assert_eq!(model["supported_reasoning_levels"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn catalog_advertises_images_only_for_vision_models() {
        let result = catalog(&[
            json!({"id": "minimax/MiniMax-M3.1-Flash", "vision": true}),
            json!({"id": "minimax/MiniMax-M2.7", "vision": false}),
            json!({"id": "unknown/model"}),
        ]);
        let models = result["models"].as_array().unwrap();
        assert_eq!(models[0]["input_modalities"], json!(["text", "image"]));
        assert_eq!(models[1]["input_modalities"], json!(["text"]));
        assert_eq!(models[2]["input_modalities"], json!(["text"]));
    }

    #[test]
    fn catalog_resolves_minimax_capabilities_when_listing_omits_them() {
        for source in crate::mmproxy::models() {
            let id = format!("minimax/{}", source["id"].as_str().unwrap());
            let result = catalog(&[json!({"id": id})]);
            let modalities = result["models"][0]["input_modalities"].as_array().unwrap();
            assert_eq!(modalities.contains(&json!("image")), source["vision"] == true);
        }
        // An explicit capability declaration wins, and names from other providers
        // must not inherit MiniMax's capabilities merely because the slug matches.
        assert!(!supports_images(&json!({"id": "minimax/MiniMax-M3", "vision": false})));
        assert!(!supports_images(&json!({"id": "other/MiniMax-M3"})));
        assert!(!supports_images(&json!({"id": "minimax/unknown"})));
    }

    #[test]
    fn catalog_with_empty_list_is_valid_json() {
        let cat = catalog(&[]);
        assert_eq!(cat["models"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn template_parses_and_has_required_fields() {
        let t = template();
        assert!(t["model_messages"].is_object(), "实测必填");
        assert!(t["use_responses_lite"].is_boolean(), "实测必填 bool");
        assert!(t["slug"].is_string());
    }
}
