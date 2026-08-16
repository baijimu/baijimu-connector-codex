use anyhow::Result;
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::process::Command;

pub const ERROR_CODE_UNSUPPORTED_OS_VERSION: &str = "UNSUPPORTED_OS_VERSION";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedOsVersion {
    platform: String,
    current_version: String,
    minimum_version: String,
    application: String,
}

impl UnsupportedOsVersion {
    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    pub fn minimum_version(&self) -> &str {
        &self.minimum_version
    }

    pub fn application(&self) -> &str {
        &self.application
    }
}

impl fmt::Display for UnsupportedOsVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{ERROR_CODE_UNSUPPORTED_OS_VERSION}: 当前 {} {} 低于 {} 要求的最低系统版本 {}，请先升级操作系统",
            self.platform, self.current_version, self.application, self.minimum_version
        )
    }
}

impl Error for UnsupportedOsVersion {}

pub fn ensure_supported(
    platform: &str,
    current_version: &str,
    minimum_version: &str,
    application: &str,
) -> Result<()> {
    let current = NumericVersion::parse(current_version)?;
    let minimum = NumericVersion::parse(minimum_version)?;
    if current.cmp(&minimum) == Ordering::Less {
        return Err(UnsupportedOsVersion {
            platform: platform.to_string(),
            current_version: current_version.to_string(),
            minimum_version: minimum_version.to_string(),
            application: application.to_string(),
        }
        .into());
    }
    Ok(())
}

pub fn validate_version(value: &str) -> Result<()> {
    NumericVersion::parse(value).map(|_| ())
}

pub fn unsupported_os_version(error: &anyhow::Error) -> Option<&UnsupportedOsVersion> {
    error
        .chain()
        .find_map(|source| source.downcast_ref::<UnsupportedOsVersion>())
}

pub fn message_is_unsupported_os_version(message: &str) -> bool {
    message.contains(ERROR_CODE_UNSUPPORTED_OS_VERSION)
}

pub fn current_macos_version() -> Result<String> {
    let output = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .map_err(|error| anyhow::anyhow!("读取当前 macOS 版本失败：{error}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "读取当前 macOS 版本失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    NumericVersion::parse(&version)?;
    Ok(version)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NumericVersion(Vec<u64>);

impl NumericVersion {
    fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            anyhow::bail!("系统版本不能为空");
        }
        let segments = value
            .split('.')
            .map(|segment| {
                if segment.is_empty() || !segment.bytes().all(|byte| byte.is_ascii_digit()) {
                    anyhow::bail!("系统版本格式无效：{value}");
                }
                segment
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("系统版本格式无效：{value}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self(segments))
    }

    fn cmp(&self, other: &Self) -> Ordering {
        let length = self.0.len().max(other.0.len());
        (0..length)
            .map(|index| {
                self.0
                    .get(index)
                    .copied()
                    .unwrap_or_default()
                    .cmp(&other.0.get(index).copied().unwrap_or_default())
            })
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_numeric_system_versions_without_string_ordering() {
        ensure_supported("macOS", "14.10", "14.2", "ChatGPT/Codex").unwrap();
        ensure_supported("Windows", "10.0.26100", "10.0.19041.0", "ChatGPT/Codex").unwrap();
        ensure_supported("macOS", "14", "14.0", "ChatGPT/Codex").unwrap();
    }

    #[test]
    fn unsupported_version_is_typed_and_non_retryable_by_callers() {
        let error = ensure_supported("macOS", "12.2.1", "14.0", "ChatGPT/Codex").unwrap_err();
        let unsupported = unsupported_os_version(&error).unwrap();

        assert_eq!(unsupported.platform(), "macOS");
        assert_eq!(unsupported.current_version(), "12.2.1");
        assert_eq!(unsupported.minimum_version(), "14.0");
        assert!(error
            .to_string()
            .contains(ERROR_CODE_UNSUPPORTED_OS_VERSION));
    }

    #[test]
    fn malformed_versions_fail_closed() {
        assert!(ensure_supported("macOS", "14.0 beta", "14.0", "ChatGPT/Codex").is_err());
        assert!(ensure_supported("macOS", "14.0", "", "ChatGPT/Codex").is_err());
    }
}
