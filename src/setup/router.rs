use reqwest::blocking::Client;
use serde::Serialize;
use std::thread;
use std::time::Duration;

const ROUTER_PROBE_ATTEMPTS: u32 = 3;
const ROUTER_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ROUTER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RouteVerification {
    attempts: u32,
    http_status: Option<u16>,
    failure: Option<String>,
}

impl RouteVerification {
    pub(super) fn passed(&self) -> bool {
        self.failure.is_none()
    }

    pub(super) fn attempts(&self) -> u32 {
        self.attempts
    }

    pub(super) fn failure_message(&self) -> Option<&str> {
        self.failure.as_deref()
    }
}

#[derive(Debug)]
struct ProbeFailure {
    message: String,
    http_status: Option<u16>,
    retryable: bool,
}

#[derive(Serialize)]
struct RouterProbeRequest<'a> {
    model: &'a str,
    input: &'a str,
}

pub(super) fn verify(credential: &str) -> RouteVerification {
    let product = crate::product_config::get();
    let endpoint = format!(
        "{}/responses",
        product.router_base_url.trim_end_matches('/')
    );
    let client = match Client::builder()
        .connect_timeout(ROUTER_CONNECT_TIMEOUT)
        .timeout(ROUTER_REQUEST_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return RouteVerification {
                attempts: 0,
                http_status: None,
                failure: Some(format!("创建百积木路由验证客户端失败：{error}")),
            };
        }
    };

    verify_with(
        ROUTER_PROBE_ATTEMPTS,
        || {
            let response = client
                .post(&endpoint)
                .bearer_auth(credential)
                .json(&RouterProbeRequest {
                    model: product.default_model.as_str(),
                    input: "Reply with exactly OK",
                })
                .send()
                .map_err(|error| ProbeFailure {
                    message: format!("路由 /responses 健康检查请求失败：{error}"),
                    http_status: None,
                    retryable: error.is_timeout() || error.is_connect() || error.is_request(),
                })?;
            let status = response.status();
            if status.as_u16() == 200 {
                return Ok(status.as_u16());
            }
            Err(ProbeFailure {
                message: format!("路由 /responses 健康检查失败：HTTP {}", status.as_u16()),
                http_status: Some(status.as_u16()),
                retryable: status.as_u16() == 408
                    || status.as_u16() == 425
                    || status.as_u16() == 429
                    || status.is_server_error(),
            })
        },
        |attempt| thread::sleep(Duration::from_secs(u64::from(attempt))),
    )
}

fn verify_with<P, S>(
    max_attempts: u32,
    mut probe: P,
    mut sleep_before_retry: S,
) -> RouteVerification
where
    P: FnMut() -> Result<u16, ProbeFailure>,
    S: FnMut(u32),
{
    assert!(
        max_attempts > 0,
        "route verification requires at least one attempt"
    );
    for attempt in 1..=max_attempts {
        match probe() {
            Ok(status) => {
                return RouteVerification {
                    attempts: attempt,
                    http_status: Some(status),
                    failure: None,
                };
            }
            Err(failure) if failure.retryable && attempt < max_attempts => {
                sleep_before_retry(attempt);
            }
            Err(failure) => {
                return RouteVerification {
                    attempts: attempt,
                    http_status: failure.http_status,
                    failure: Some(failure.message),
                };
            }
        }
    }
    unreachable!("positive attempt count always returns")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transient(message: &str) -> ProbeFailure {
        ProbeFailure {
            message: message.to_string(),
            http_status: None,
            retryable: true,
        }
    }

    #[test]
    fn transient_route_failures_are_retried_until_the_probe_passes() {
        let mut calls = 0;
        let mut delays = Vec::new();
        let result = verify_with(
            3,
            || {
                calls += 1;
                if calls < 3 {
                    Err(transient("temporary timeout"))
                } else {
                    Ok(200)
                }
            },
            |attempt| delays.push(attempt),
        );

        assert!(result.passed());
        assert_eq!(result.attempts(), 3);
        assert_eq!(result.http_status, Some(200));
        assert_eq!(delays, vec![1, 2]);
    }

    #[test]
    fn authorization_failures_are_not_retried() {
        let mut calls = 0;
        let result = verify_with(
            3,
            || {
                calls += 1;
                Err(ProbeFailure {
                    message: "HTTP 401".to_string(),
                    http_status: Some(401),
                    retryable: false,
                })
            },
            |_| panic!("a permanent failure must not sleep"),
        );

        assert!(!result.passed());
        assert_eq!(calls, 1);
        assert_eq!(result.attempts(), 1);
        assert_eq!(result.http_status, Some(401));
    }

    #[test]
    fn the_last_transient_error_is_reported_after_all_attempts() {
        let mut calls = 0;
        let result = verify_with(
            3,
            || {
                calls += 1;
                Err(transient(&format!("timeout {calls}")))
            },
            |_| {},
        );

        assert!(!result.passed());
        assert_eq!(result.attempts(), 3);
        assert_eq!(result.failure_message(), Some("timeout 3"));
    }
}
