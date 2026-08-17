use serde::Deserialize;
use std::sync::OnceLock;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProductConfig {
    schema_version: u32,
    pub(crate) default_model: String,
    pub(crate) router_provider: String,
    pub(crate) router_base_url: String,
}

pub(crate) fn get() -> &'static ProductConfig {
    static CONFIG: OnceLock<ProductConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let config = crate::json_compat::from_slice::<ProductConfig>(include_bytes!(
            "../config/codex-workspace-profile.json"
        ))
        .expect("Codex 桌面工作区配置必须是有效 JSON");
        assert_eq!(
            config.schema_version, SCHEMA_VERSION,
            "Codex 桌面工作区配置版本无效"
        );
        assert!(!config.default_model.trim().is_empty(), "默认模型不能为空");
        assert!(
            !config.router_provider.trim().is_empty(),
            "路由 provider 不能为空"
        );
        assert!(
            config.router_base_url.starts_with("https://"),
            "路由地址必须使用 HTTPS"
        );
        config
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_product_config_is_valid() {
        let config = get();
        assert_eq!(config.schema_version, SCHEMA_VERSION);
        assert!(config.router_base_url.starts_with("https://"));
    }
}
