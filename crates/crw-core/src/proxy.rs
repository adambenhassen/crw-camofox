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

/// `text` with the userinfo of every `scheme://` URL inside it masked, via
/// [`redact_proxy_url`].
///
/// For free-form text that may quote a proxy URL somewhere in the middle, such
/// as an error cause chain about to be logged. A URL runs from its scheme to the
/// next whitespace, deliberately ignoring `)`, `,` and quotes: those are legal in
/// an unencoded password, and stopping at one would leave the rest of the
/// password after the cut in the clear.
pub(crate) fn redact_url_userinfo(text: &str) -> String {
    let is_scheme_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find("://") {
        // Scheme bytes are ASCII, so stopping on any other byte (including the
        // tail of a multi-byte char) always lands on a char boundary.
        let start = rest.as_bytes()[..sep]
            .iter()
            .rposition(|&b| !is_scheme_byte(b))
            .map_or(0, |i| i + 1);
        let end = rest[sep..]
            .find(char::is_whitespace)
            .map_or(rest.len(), |i| sep + i);
        out.push_str(&rest[..start]);
        out.push_str(&redact_proxy_url(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Check that a proxy URL is one the HTTP client accepts. A malformed value
/// used to be logged and dropped, so traffic meant for the proxy went out
/// directly from the server's own address. The error redacts credentials.
pub fn validate_proxy_url(raw: &str) -> Result<(), String> {
    reqwest::Proxy::all(raw.trim()).map(|_| ()).map_err(|e| {
        format!(
            "Invalid proxy URL '{}': {}",
            redact_proxy_url(raw),
            crate::error::reqwest_message(e)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_userinfo_masks_every_url_in_free_text() {
        // The shape a proxy failure takes once its cause chain is flattened.
        assert_eq!(
            redact_url_userinfo(
                "error sending request: proxy http://user:s3cret@gw.example.com:823 refused"
            ),
            "error sending request: proxy http://***@gw.example.com:823 refused"
        );
        // Wrapped in parens, two URLs, a `)` inside the password: nothing of
        // the password survives, the surrounding text does.
        let out = redact_url_userinfo("a (socks5h://u:p)w@h1:1080) then https://v:x@h2/path, ok");
        assert!(!out.contains("p)w"), "{out}");
        assert!(!out.contains("v:x"), "{out}");
        assert_eq!(
            out,
            "a (socks5h://***@h1:1080) then https://***@h2/path, ok"
        );
        // No URL, or a URL without credentials: unchanged, multi-byte text too.
        assert_eq!(
            redact_url_userinfo("tunnel error: unsuccessful"),
            "tunnel error: unsuccessful"
        );
        assert_eq!(
            redact_url_userinfo("çağrı http://gw.example.com:823 bitti"),
            "çağrı http://gw.example.com:823 bitti"
        );
        assert_eq!(redact_url_userinfo("://u:p@h"), "***@h");
        assert_eq!(redact_url_userinfo(""), "");
    }

    #[test]
    fn validate_proxy_url_accepts_supported_schemes() {
        assert!(validate_proxy_url("http://proxy:8080").is_ok());
        assert!(validate_proxy_url("socks5://user:pass@proxy:1080").is_ok());
    }

    #[test]
    fn validate_proxy_url_rejects_without_quoting_credentials() {
        let err = validate_proxy_url("http://user:hunter2@[not-a-host").unwrap_err();
        assert!(err.contains("Invalid proxy URL"), "{err}");
        assert!(
            !err.contains("hunter2"),
            "must not leak the password: {err}"
        );
        assert!(!err.contains("user:"), "must not leak the username: {err}");
    }

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
