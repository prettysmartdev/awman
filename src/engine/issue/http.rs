//! Allowlisted HTTP work items: GET JSON/markdown, POST `{title,body}`,
//! PATCH the fixed issue URL with `{body}` (JSON response or 204).
use super::{transport, Issue, IssueOperation, IssueSource, IssueSourceError};
use crate::data::config::{
    env::EnvSnapshot,
    work_items::{is_loopback_host, validate_provider_name, WorkItemProviderConfig},
};
use reqwest::{Method, Url};
use serde::Deserialize;
use std::path::Path;

pub struct HttpIssueSource {
    provider: String,
    base: Url,
    token: Option<String>,
    token_env: String,
}

fn unsafe_url(provider: &str, reason: &str) -> IssueSourceError {
    IssueSourceError::UnsafeUrl {
        provider: provider.into(),
        reason: reason.into(),
    }
}

/// Decode a single percent layer without treating a literal `%` in an ID as
/// an escape. URL spelling is checked separately before this is used.
fn decode(value: &str) -> String {
    let mut bytes = Vec::new();
    let mut input = value.as_bytes().iter().copied().peekable();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let mut next = input.clone();
            if let (Some(a), Some(b)) = (next.next(), next.next()) {
                if let (Some(a), Some(b)) = ((a as char).to_digit(16), (b as char).to_digit(16)) {
                    bytes.push((a * 16 + b) as u8);
                    input = next;
                    continue;
                }
            }
        }
        bytes.push(byte);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn safe_id(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut current = value.to_string();
    for _ in 0..16 {
        if current.contains('\\')
            || current.chars().any(char::is_control)
            || current.split('/').any(|part| matches!(part, "." | ".."))
        {
            return false;
        }
        let decoded = decode(&current);
        if decoded == current {
            return true;
        }
        current = decoded;
    }
    false
}

fn valid_escapes(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Check raw paths before Url's parser normalizes literal/encoded dot segments.
/// Parsed references have already lost that spelling, but containment is still
/// checked on the resulting destination before any request.
pub(super) fn validate_raw_url(input: &str, provider: &str) -> Result<(), IssueSourceError> {
    let remainder = input
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(input);
    let before_query = remainder.split(['?', '#']).next().unwrap_or_default();
    if before_query.contains('\\') || input.chars().any(char::is_control) {
        return Err(unsafe_url(provider, "ambiguous or malformed URL"));
    }
    let path = before_query
        .find('/')
        .map(|n| &before_query[n..])
        .unwrap_or("");
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if path.contains('\\') || !valid_escapes(path) || (!path.is_empty() && !safe_id(path)) {
        return Err(unsafe_url(provider, "ambiguous or malformed path"));
    }
    Ok(())
}

pub(super) fn validate_url(url: &Url, provider: &str) -> Result<(), IssueSourceError> {
    let loopback = is_loopback_host(url);
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(unsafe_url(provider, "https is required except on loopback"));
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(unsafe_url(
            provider,
            "host is required and userinfo is forbidden",
        ));
    }
    if !valid_escapes(url.path()) || !safe_id(url.path()) {
        return Err(unsafe_url(provider, "ambiguous or malformed path"));
    }
    Ok(())
}

fn segments(url: &Url) -> Vec<String> {
    let mut parts: Vec<_> = url
        .path_segments()
        .into_iter()
        .flatten()
        .map(decode)
        .collect();
    while parts.last().is_some_and(String::is_empty) {
        parts.pop();
    }
    parts
}

pub fn url_matches_base(base: &Url, candidate: &Url) -> bool {
    if validate_url(base, "HTTP").is_err() || validate_url(candidate, "HTTP").is_err() {
        return false;
    }
    // A repeated separator in an authority boundary is ambiguous across
    // servers that collapse empty segments. Never widen that configured scope.
    if base.path().contains("//") {
        return false;
    }
    let base_parts = segments(base);
    // Separators in the base could move its boundary when decoded by a server.
    if base_parts.iter().any(|part| {
        let mut part = part.clone();
        loop {
            if part.contains('/') || part.contains('\\') {
                return true;
            }
            let next = decode(&part);
            if next == part {
                return false;
            }
            part = next;
        }
    }) {
        return false;
    }
    base.scheme() == candidate.scheme()
        && base.host() == candidate.host()
        && base.port_or_known_default() == candidate.port_or_known_default()
        && segments(candidate).starts_with(&base_parts)
}

pub fn canonical_work_item_url(url: &Url) -> Result<String, IssueSourceError> {
    validate_url(url, "HTTP")?;
    let mut url = url.clone();
    url.set_fragment(None);
    Ok(url.into())
}

impl HttpIssueSource {
    pub fn new(
        name: &str,
        config: &WorkItemProviderConfig,
        env: &EnvSnapshot,
    ) -> Result<Self, IssueSourceError> {
        let provider = format!("http:{name}");
        validate_provider_name(name)
            .map_err(|_| transport::error(&provider, "invalid provider name"))?;
        let mut env_chars = config.token_env.chars();
        if !env_chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            || !env_chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        {
            return Err(transport::error(
                &provider,
                "invalid token environment name",
            ));
        }
        validate_raw_url(&config.base_url, &provider)?;
        let mut base =
            Url::parse(&config.base_url).map_err(|_| unsafe_url(&provider, "invalid base URL"))?;
        validate_url(&base, &provider)?;
        if base.query().is_some() || base.fragment().is_some() || !url_matches_base(&base, &base) {
            return Err(unsafe_url(
                &provider,
                "base URL must have an unambiguous path and no query or fragment",
            ));
        }
        let path = base.path().trim_end_matches('/').to_string();
        base.set_path(&path);
        Ok(Self {
            provider,
            base,
            token: env
                .get(&config.token_env)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            token_env: config.token_env.clone(),
        })
    }
    pub fn base_url(&self) -> &Url {
        &self.base
    }
    pub fn url_for_id(&self, id: &str) -> Result<Url, IssueSourceError> {
        if !safe_id(id) {
            return Err(unsafe_url(&self.provider, "unsafe logical work item ID"));
        }
        let mut encoded = String::new();
        for byte in id.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                encoded.push(byte as char);
            } else {
                use std::fmt::Write;
                let _ = write!(&mut encoded, "%{byte:02X}");
            }
        }
        let mut url = self.base.clone();
        url.set_path(&format!(
            "{}/{encoded}",
            self.base.path().trim_end_matches('/')
        ));
        self.allowed_url(url.as_str())
    }
    fn allowed_url(&self, input: &str) -> Result<Url, IssueSourceError> {
        validate_raw_url(input, &self.provider)?;
        let mut url =
            Url::parse(input).map_err(|_| unsafe_url(&self.provider, "invalid work item URL"))?;
        validate_url(&url, &self.provider)?;
        if !url_matches_base(&self.base, &url) {
            return Err(unsafe_url(
                &self.provider,
                "URL does not match configured baseUrl",
            ));
        }
        url.set_fragment(None);
        Ok(url)
    }
    fn request(
        &self,
        method: Method,
        url: &Url,
        payload: Option<serde_json::Value>,
        operation: IssueOperation,
    ) -> Result<reqwest::blocking::Response, IssueSourceError> {
        // Validation always precedes credentials and network access.
        self.allowed_url(url.as_str())?;
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| IssueSourceError::Unauthorized {
                provider: self.provider.clone(),
                hint: format!("set {} for this provider", self.token_env),
            })?;
        transport::outbound(&self.provider, url)?;
        let client = transport::client_for(&self.provider, url)?;
        let mut request = client
            .request(method, url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(token);
        if let Some(payload) = payload {
            request = request.json(&payload);
        }
        let response = request
            .send()
            .map_err(|e| transport::request_error(&self.provider, operation, e))?;
        transport::status(
            &self.provider,
            &response,
            url.as_str(),
            "check the provider token",
            operation,
        )?;
        Ok(response)
    }
    fn json_issue(&self, bytes: &[u8], source: Option<&Url>) -> Result<Issue, IssueSourceError> {
        let value: HttpIssue = serde_json::from_slice(bytes)
            .map_err(|_| transport::error(&self.provider, "invalid issue JSON response"))?;
        let id_url = self.url_for_id(&value.id)?;
        Ok(Issue {
            source_id: canonical_work_item_url(source.unwrap_or(&id_url))?,
            id: value.id,
            number: value.number,
            title: value.title,
            body: value.body,
            provider: self.provider.clone(),
        })
    }
}
#[derive(Deserialize)]
struct HttpIssue {
    id: String,
    title: String,
    body: String,
    number: Option<u32>,
}

fn markdown_title(body: &str) -> String {
    let mut previous = "";
    let mut fence = None;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            let marker = line.chars().next().expect("fence prefix");
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
            previous = "";
            continue;
        }
        if fence.is_some() {
            continue;
        }
        let count = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&count)
            && line
                .as_bytes()
                .get(count)
                .is_some_and(u8::is_ascii_whitespace)
        {
            return line[count..]
                .trim()
                .trim_end_matches('#')
                .trim()
                .to_string();
        }
        if !previous.is_empty()
            && !line.is_empty()
            && (line.chars().all(|c| c == '=') || line.chars().all(|c| c == '-'))
        {
            return previous.to_string();
        }
        previous = line;
    }
    "Untitled work item".into()
}

impl IssueSource for HttpIssueSource {
    fn provider_name(&self) -> &str {
        &self.provider
    }
    fn provider_prefix(&self) -> &str {
        "http"
    }
    fn issue_identifier(&self, issue: &Issue) -> String {
        issue.id.clone()
    }
    fn fetch_issue(&self, input: &str, _git_root: &Path) -> Result<Issue, IssueSourceError> {
        let url = self.allowed_url(input)?;
        let response = self.request(Method::GET, &url, None, IssueOperation::Fetch)?;
        let markdown = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/markdown"))
            });
        let bytes = transport::read_response(&self.provider, response, IssueOperation::Fetch)?;
        if !markdown {
            return self.json_issue(&bytes, Some(&url));
        }
        let body = String::from_utf8(bytes)
            .map_err(|_| transport::error(&self.provider, "markdown response is not UTF-8"))?;
        let title = markdown_title(&body);
        let id = decode(
            url.path_segments()
                .and_then(|mut p| p.next_back())
                .unwrap_or_default(),
        );
        if !safe_id(&id) {
            return Err(unsafe_url(&self.provider, "unsafe work item ID"));
        }
        Ok(Issue {
            source_id: canonical_work_item_url(&url)?,
            id,
            number: None,
            title,
            body,
            provider: self.provider.clone(),
        })
    }
    fn create_issue(
        &self,
        title: &str,
        body: &str,
        _git_root: &Path,
    ) -> Result<Issue, IssueSourceError> {
        transport::check_body(&self.provider, body)?;
        let response = self.request(
            Method::POST,
            &self.base,
            Some(serde_json::json!({"title":title,"body":body})),
            IssueOperation::Create,
        )?;
        let bytes = transport::read_response(&self.provider, response, IssueOperation::Create)?;
        self.json_issue(&bytes, None)
            .map_err(|_| transport::unknown(&self.provider))
    }
    fn update_issue(&self, issue: &Issue, body: &str) -> Result<Issue, IssueSourceError> {
        transport::check_body(&self.provider, body)?;
        if issue.provider != self.provider {
            return Err(transport::error(
                &self.provider,
                "issue belongs to another provider",
            ));
        }
        let url = self.allowed_url(&issue.source_id)?;
        let response = self.request(
            Method::PATCH,
            &url,
            Some(serde_json::json!({"body":body})),
            IssueOperation::Update,
        )?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(Issue {
                body: body.into(),
                ..issue.clone()
            });
        }
        let bytes = transport::read_response(&self.provider, response, IssueOperation::Update)?;
        let result = self.json_issue(&bytes, Some(&url))?;
        if result.id != issue.id || result.number != issue.number {
            return Err(transport::error(
                &self.provider,
                "update response changed work item identity",
            ));
        }
        Ok(Issue {
            source_id: issue.source_id.clone(),
            ..result
        })
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;
    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }
    #[test]
    fn markdown_title_accepts_headings_and_ignores_fenced_examples() {
        assert_eq!(
            markdown_title("```md\n# Example\n```\n\n## Actual heading"),
            "Actual heading"
        );
        assert_eq!(
            markdown_title("Setext title\n============\n\nbody"),
            "Setext title"
        );
        assert_eq!(markdown_title("plain text"), "Untitled work item");
    }
    #[test]
    fn containment_uses_origin_and_whole_decoded_segments() {
        let base = url("https://A.example:443/api/items/");
        for candidate in [
            "https://a.example/api/items",
            "https://a.example/api/items/42",
            "https://a.example/api/%69tems/42",
        ] {
            assert!(url_matches_base(&base, &url(candidate)), "{candidate}");
        }
        for candidate in [
            "https://a.example.evil.com/api/items/42",
            "https://a.example:444/api/items/42",
            "http://a.example/api/items/42",
            "https://a.example/api/items-evil/42",
            "https://u:p@a.example/api/items/42",
            "https://a.example/api/items/%252e%252e%252fprivate",
            "https://a.example/api/items/a%5Cb",
            "https://a.example/api/items/%ZZ",
        ] {
            assert!(!url_matches_base(&base, &url(candidate)), "{candidate}");
        }
        assert!(!url_matches_base(
            &url("https://a.example/api%2Fitems"),
            &url("https://a.example/api%2Fitems/1")
        ));
        assert!(!url_matches_base(
            &url("https://a.example//"),
            &url("https://a.example/private")
        ));
    }
    #[test]
    fn ids_are_one_encoded_segment_and_traversal_is_rejected() {
        let source = HttpIssueSource::new(
            "example",
            &WorkItemProviderConfig {
                base_url: "https://a.example/items".into(),
                token_env: "TOKEN".into(),
            },
            &EnvSnapshot::empty(),
        )
        .unwrap();
        assert_eq!(
            source.url_for_id("é/a?b#c%:").unwrap().as_str(),
            "https://a.example/items/%C3%A9%2Fa%3Fb%23c%25%3A"
        );
        for id in [".", "..", "a/../b", "a\\b", "%252e%252e", "a%252f..%252fb"] {
            assert!(source.url_for_id(id).is_err(), "{id}");
        }
        for input in [
            "https://a.example/items/../x",
            "https://a.example/items/%2e%2e/x",
            "https://a.example/items/%",
        ] {
            assert!(validate_raw_url(input, "HTTP").is_err());
        }
    }
    #[test]
    fn transport_requires_https_except_loopback() {
        for input in [
            "http://localhost/items",
            "http://127.0.0.1/items",
            "http://[::1]/items",
        ] {
            assert!(validate_url(&url(input), "HTTP").is_ok());
        }
        assert!(validate_url(&url("http://a.example/items"), "HTTP").is_err());
    }
}
