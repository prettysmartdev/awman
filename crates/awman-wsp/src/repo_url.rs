//! Repository addresses never carry credentials. Validate before storing or logging.
pub fn validate(url: &str) -> Result<(), &'static str> {
    if url.is_empty()
        || url
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        || url.starts_with('-')
        || url.contains(['?', '#'])
    {
        return Err("invalid lease repository URL; credentials belong in repo.credentials");
    }
    if url.contains("://") {
        let parsed = reqwest::Url::parse(url).map_err(|_| "invalid lease repository URL")?;
        if parsed.host_str().is_none()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err("invalid lease repository URL; credentials belong in repo.credentials");
        }
        match parsed.scheme() {
            "https" if parsed.username().is_empty() => Ok(()),
            "http"
                if parsed.username().is_empty()
                    && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) =>
            {
                Ok(())
            }
            "ssh" => Ok(()),
            _ => Err("lease repository URL must use HTTPS or SSH"),
        }
    } else if let Some((host, path)) = url.split_once(':') {
        if !host.is_empty()
            && !host.contains(['/', ':'])
            && !path.is_empty()
            && !path.starts_with(':')
            && host.split('@').count() <= 2
        {
            Ok(())
        } else {
            Err("invalid lease SSH repository URL")
        }
    } else {
        Err("lease repository URL must be remote")
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn rejects_credentials_and_query_urls_before_storage_or_transport() {
        for url in [
            "ssh://git:SECRET@git.example/r",
            "https://git.example/r?access_token=SECRET",
            "ssh://git@git.example/r#SECRET",
            "https://SECRET@git.example/r",
            "ext::sh -c id",
        ] {
            let error = super::validate(url).unwrap_err();
            assert!(!error.contains("SECRET"));
        }
        for url in [
            "ssh://git@git.example/r",
            "git@git.example:r",
            "https://git.example/r",
            "http://[::1]:8080/r",
        ] {
            assert!(super::validate(url).is_ok(), "{url}");
        }
    }
}
