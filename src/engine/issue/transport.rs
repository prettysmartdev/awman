//! Shared bounded HTTP mechanics; never renders upstream error bodies.
use super::{IssueOperation, IssueSourceError};
use crate::data::fs::work_item_draft::MAX_WORK_ITEM_BYTES;
use reqwest::{
    blocking::{Client, Response},
    Url,
};
use std::io::Read;
use std::time::Duration;

pub(super) const RESPONSE_LIMIT: u64 = 2 * 1024 * 1024;
pub(super) fn error(provider: &str, detail: &str) -> IssueSourceError {
    IssueSourceError::ProviderError {
        provider: provider.into(),
        detail: detail.into(),
    }
}
pub(super) fn unknown(provider: &str) -> IssueSourceError {
    IssueSourceError::CreateOutcomeUnknown {
        provider: provider.into(),
        detail: "the request may have created an issue".into(),
    }
}
pub(super) fn check_body(provider: &str, body: &str) -> Result<(), IssueSourceError> {
    if body.len() as u64 > MAX_WORK_ITEM_BYTES {
        return Err(error(
            provider,
            "issue body exceeds the work item size limit",
        ));
    }
    Ok(())
}
pub(super) fn client(provider: &str) -> Result<Client, IssueSourceError> {
    builder()
        .build()
        .map_err(|_| error(provider, "could not construct HTTP client"))
}

/// A client for one request to `url`. Plain `http` is allowed only on
/// loopback, so it never goes through a proxy (the bearer token would leave the
/// host in clear text) and `localhost` is pinned to 127.0.0.1 rather than
/// resolved by the system resolver.
pub(super) fn client_for(provider: &str, url: &Url) -> Result<Client, IssueSourceError> {
    let mut builder = builder();
    if url.scheme() == "http" {
        builder = builder.no_proxy();
        if url
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case("localhost"))
        {
            let port = url.port_or_known_default().unwrap_or(80);
            builder = builder.resolve(
                "localhost",
                std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            );
        }
    }
    builder
        .build()
        .map_err(|_| error(provider, "could not construct HTTP client"))
}

fn builder() -> reqwest::blocking::ClientBuilder {
    Client::builder()
        .user_agent("awman")
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
}
pub(super) fn outbound(provider: &str, url: &Url) -> Result<(), IssueSourceError> {
    crate::engine::remote::HttpCore::refuse_public_host_under_test_isolation(url.as_str()).map_err(
        |_| IssueSourceError::Network {
            provider: provider.into(),
            detail: "destination disabled under test isolation".into(),
        },
    )
}
pub(super) fn request_error(
    provider: &str,
    operation: IssueOperation,
    e: reqwest::Error,
) -> IssueSourceError {
    if operation == IssueOperation::Create && !e.is_connect() && !e.is_builder() {
        return unknown(provider);
    }
    if e.is_timeout() {
        return IssueSourceError::Timeout {
            provider: provider.into(),
            operation,
        };
    }
    IssueSourceError::Network {
        provider: provider.into(),
        detail: "HTTP request failed".into(),
    }
}
pub(super) fn status(
    provider: &str,
    response: &Response,
    source_id: &str,
    auth_hint: &str,
    operation: IssueOperation,
) -> Result<(), IssueSourceError> {
    let code = response.status().as_u16();
    if response.status().is_success() {
        return Ok(());
    }
    Err(match code {
        401 => IssueSourceError::Unauthorized {
            provider: provider.into(),
            hint: auth_hint.into(),
        },
        403 if response
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0")
            || response.headers().contains_key("retry-after") =>
        {
            IssueSourceError::RateLimited {
                provider: provider.into(),
            }
        }
        403 => IssueSourceError::Forbidden {
            provider: provider.into(),
            hint: "check provider write permissions".into(),
        },
        404 => IssueSourceError::NotFound {
            provider: provider.into(),
            source_id: source_id.into(),
        },
        429 => IssueSourceError::RateLimited {
            provider: provider.into(),
        },
        300..=399 | 408 | 500..=599 if operation == IssueOperation::Create => unknown(provider),
        _ => error(
            provider,
            &format!("HTTP {code}; redirects are not followed"),
        ),
    })
}
pub(super) fn read_response(
    provider: &str,
    response: Response,
    operation: IssueOperation,
) -> Result<Vec<u8>, IssueSourceError> {
    let failure = || {
        if operation == IssueOperation::Create {
            unknown(provider)
        } else {
            error(provider, "could not read response")
        }
    };
    if response
        .content_length()
        .is_some_and(|n| n > RESPONSE_LIMIT)
    {
        return Err(if operation == IssueOperation::Create {
            unknown(provider)
        } else {
            IssueSourceError::ResponseTooLarge {
                provider: provider.into(),
                limit: RESPONSE_LIMIT,
            }
        });
    }
    let mut bytes = Vec::new();
    response
        .take(RESPONSE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| {
            if operation == IssueOperation::Create {
                unknown(provider)
            } else if e.kind() == std::io::ErrorKind::TimedOut
                || e.get_ref()
                    .and_then(|source| source.downcast_ref::<reqwest::Error>())
                    .is_some_and(reqwest::Error::is_timeout)
            {
                IssueSourceError::Timeout {
                    provider: provider.into(),
                    operation,
                }
            } else {
                failure()
            }
        })?;
    if bytes.len() as u64 > RESPONSE_LIMIT {
        return Err(if operation == IssueOperation::Create {
            unknown(provider)
        } else {
            IssueSourceError::ResponseTooLarge {
                provider: provider.into(),
                limit: RESPONSE_LIMIT,
            }
        });
    }
    Ok(bytes)
}
