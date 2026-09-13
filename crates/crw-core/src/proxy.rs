//! Proxy URL helpers.

/// `raw` with any `user:pass@` userinfo replaced by `***@`.
///
/// Proxy URLs carry credentials and end up in `ConfigError` messages that are
/// returned to API callers verbatim, so no error string may ever quote a proxy
/// URL directly. Purely textual on purpose: it must also redact a value that
/// failed to parse as a URL, which is exactly when these errors fire.
pub fn redact_proxy_url(raw: &str) -> String {
    let trimmed = raw.trim();
    // Keep the scheme only when it is one (RFC 3986: a letter followed by
    // letters, digits, `+`, `-`, `.`). Anything else before `://` may itself be
    // the credentials of a mangled value, so it is masked with the rest.
    let is_scheme = |s: &str| {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    };
    let (prefix, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) if is_scheme(scheme) => (format!("{scheme}://"), rest),
        _ => (String::new(), trimmed),
    };
    // Last `@` wins: a host never contains one, an unencoded password can.
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{prefix}***@{host}"),
        None => format!("{prefix}{rest}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_proxy_url_masks_userinfo() {
        assert_eq!(
            redact_proxy_url("http://user:s3cret@gw.example.com:823"),
            "http://***@gw.example.com:823"
        );
        // No credentials: unchanged.
        assert_eq!(
            redact_proxy_url("socks5h://gw.example.com:1080"),
            "socks5h://gw.example.com:1080"
        );
        // A typo'd scheme still parses out, which is the common `--proxy htp://`
        // case an operator needs to see.
        assert_eq!(redact_proxy_url("htp://user:pw@host"), "htp://***@host");
        // A value too mangled to split on `://` loses the prefix rather than
        // risking the credentials: masking wins over fidelity here.
        assert_eq!(redact_proxy_url("htp:/user:pw@host"), "***@host");
        // A "scheme" that is not one is credentials in disguise: masked too.
        assert_eq!(
            redact_proxy_url("user:s3cret://tail@host:8080"),
            "***@host:8080"
        );
        assert_eq!(redact_proxy_url(""), "");
        // An `@` in the path of a credential-free value is masked too: masking
        // wins over fidelity, and this documents that it is on purpose.
        assert_eq!(
            redact_proxy_url("http://gw.example.com:823/a@b"),
            "http://***@b"
        );
        // A literal `@` in the password does not fool the split.
        assert_eq!(
            redact_proxy_url("http://user:p@ss@host:8080"),
            "http://***@host:8080"
        );
    }
}
