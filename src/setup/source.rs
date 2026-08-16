use anyhow::{Context, Result};
use serde::Deserialize;

const SOURCE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpstreamArtifactSourceV2 {
    schema_version: u32,
    manifest_url: String,
}

pub(super) fn manifest_url() -> Result<String> {
    let source = crate::json_compat::from_slice::<UpstreamArtifactSourceV2>(include_bytes!(
        "../../installers/upstream-artifact-source.json"
    ))
    .context("解析安装制品源配置失败")?;
    if source.schema_version != SOURCE_SCHEMA_VERSION {
        anyhow::bail!("不支持的安装制品源配置版本：{}", source.schema_version);
    }
    let url = source.manifest_url.trim();
    if !(url.starts_with("https://") && url.ends_with(".json")) {
        anyhow::bail!("安装制品清单地址必须是 HTTPS JSON 地址");
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_source_is_a_closed_versioned_contract() {
        assert_eq!(
            manifest_url().unwrap(),
            "https://download.baijimu.com/codex-artifacts/v4/latest.json"
        );
        assert!(serde_json::from_str::<UpstreamArtifactSourceV2>(
            r#"{"schemaVersion":2,"manifestUrl":"https://example.com/latest.json","extra":true}"#
        )
        .is_err());
    }
}
