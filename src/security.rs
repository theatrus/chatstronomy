//! Credential-safe diagnostics shared by chat, HTTP, and Direct transports.

use std::fmt;
use url::Url;

const REDACTED: &str = "[redacted]";

/// Render a secret's presence without ever including its contents.
pub(crate) fn secret_marker(value: &str) -> &'static str {
    if value.is_empty() {
        "<empty>"
    } else {
        REDACTED
    }
}

/// Remove credentials from URLs and common authorization/credential fields.
///
/// Request URLs frequently appear inside third-party error messages rather
/// than as structured values. Drop every query and fragment, strip userinfo,
/// and replace the secret path segment of Discord webhook endpoints while
/// retaining the host and safe route for troubleshooting.
pub fn redact_sensitive(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut remainder = message;

    while let Some((index, scheme)) = next_url(remainder) {
        output.push_str(&remainder[..index]);
        let candidate = &remainder[index..];
        let end = candidate
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ')' | ']' | '}' | '>' | '"' | '\'')
            })
            .unwrap_or(candidate.len());
        let raw = &candidate[..end];
        if raw.len() > scheme.len()
            && let Ok(url) = Url::parse(raw)
        {
            output.push_str(&safe_url(&url));
        } else {
            output.push_str(raw);
        }
        remainder = &candidate[end..];
    }
    output.push_str(remainder);

    let mut redacted = output;
    for scheme in ["bearer", "basic", "bot"] {
        redacted = redact_authorization_scheme(&redacted, scheme);
    }
    for field in [
        "access_token",
        "refresh_token",
        "id_token",
        "bot_token",
        "pairing_token",
        "csrf_token",
        "client_secret",
        "api_key",
        "apikey",
        "password",
        "passwd",
        "authorization",
        "credential",
        "secret",
        "token",
    ] {
        redacted = redact_named_value(&redacted, field);
    }
    redacted
}

fn next_url(value: &str) -> Option<(usize, &'static str)> {
    ["https://", "http://", "wss://", "ws://"]
        .into_iter()
        .filter_map(|scheme| value.find(scheme).map(|index| (index, scheme)))
        .min_by_key(|(index, _)| *index)
}

fn safe_url(url: &Url) -> String {
    let mut safe = url.clone();
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    safe.set_query(None);
    safe.set_fragment(None);

    let segments: Vec<String> = safe
        .path_segments()
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect();
    if let Some(index) = segments
        .iter()
        .position(|segment| segment.eq_ignore_ascii_case("webhooks"))
        && segments.len() > index + 1
    {
        // Even a webhook route identifies the type of credential in logs.
        // Keep only the safe API prefix; never expose the route, ID, or token.
        let prefix = segments[..index].join("/");
        let safe_path = if prefix.is_empty() {
            format!("/{REDACTED}")
        } else {
            format!("/{prefix}/{REDACTED}")
        };
        safe.set_path(&safe_path);
        // `Url` percent-encodes brackets; a plain marker reads more clearly.
        return safe.as_str().replace("%5Bredacted%5D", REDACTED);
    }

    safe.to_string()
}

fn redact_authorization_scheme(value: &str, scheme: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let pattern = format!("{scheme} ");
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(offset) = lower[cursor..].find(&pattern) {
        let start = cursor + offset;
        let secret_start = start + pattern.len();
        let secret_end = value[secret_start..]
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, ',' | ';' | '"' | '\'' | ')' | ']' | '}')
            })
            .map_or(value.len(), |end| secret_start + end);

        if secret_end == secret_start {
            result.push_str(&value[cursor..secret_start]);
            cursor = secret_start;
            continue;
        }

        result.push_str(&value[cursor..secret_start]);
        result.push_str(REDACTED);
        cursor = secret_end;
    }
    result.push_str(&value[cursor..]);
    result
}

fn redact_named_value(value: &str, field: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let mut result = String::with_capacity(value.len());
    let mut cursor = 0;

    while let Some(offset) = lower[cursor..].find(field) {
        let start = cursor + offset;
        if start > 0 && value.as_bytes()[start - 1].is_ascii_alphanumeric() {
            result.push_str(&value[cursor..start + field.len()]);
            cursor = start + field.len();
            continue;
        }

        let mut separator = start + field.len();
        while let Some(character) = value[separator..].chars().next() {
            if character == '"' || character == '\'' || character.is_whitespace() {
                separator += character.len_utf8();
            } else {
                break;
            }
        }
        if !matches!(value[separator..].chars().next(), Some(':') | Some('=')) {
            result.push_str(&value[cursor..start + field.len()]);
            cursor = start + field.len();
            continue;
        }
        separator += 1;
        while let Some(character) = value[separator..].chars().next() {
            if character.is_whitespace() {
                separator += character.len_utf8();
            } else {
                break;
            }
        }

        let quote = value[separator..]
            .chars()
            .next()
            .filter(|character| matches!(character, '"' | '\''));
        let secret_start = separator + usize::from(quote.is_some());
        let secret_end = value[secret_start..]
            .find(|character: char| {
                quote.map_or_else(
                    || {
                        character.is_whitespace()
                            || matches!(character, ',' | ';' | '&' | ')' | ']' | '}')
                    },
                    |delimiter| character == delimiter,
                )
            })
            .map_or(value.len(), |end| secret_start + end);

        result.push_str(&value[cursor..secret_start]);
        result.push_str(REDACTED);
        cursor = secret_end;
    }
    result.push_str(&value[cursor..]);
    result
}

/// An HTTP error that cannot reveal URL userinfo, queries, or webhook secrets
/// through either `Display`, `Debug`, or its underlying error chain.
pub struct SafeHttpError {
    error: reqwest::Error,
    endpoint: Option<String>,
}

impl From<reqwest::Error> for SafeHttpError {
    fn from(error: reqwest::Error) -> Self {
        let endpoint = error.url().map(safe_url);
        Self {
            error: error.without_url(),
            endpoint,
        }
    }
}

impl fmt::Display for SafeHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", redact_sensitive(&self.error.to_string()))?;
        if let Some(endpoint) = &self.endpoint {
            write!(formatter, " ({endpoint})")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SafeHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SafeHttpError")
            .field(&self.to_string())
            .finish()
    }
}

// Do not expose the original reqwest error chain: connector/proxy sources are
// third-party values and might print the original authenticated endpoint.
impl std::error::Error for SafeHttpError {}

/// A third-party error whose original chain may contain credentials in URLs,
/// request headers, or response bodies. Preserve only its sanitized message.
pub struct SanitizedError {
    message: String,
}

impl SanitizedError {
    pub fn new(error: impl fmt::Display) -> Self {
        Self {
            message: redact_sensitive(&error.to_string()),
        }
    }
}

impl fmt::Display for SanitizedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl fmt::Debug for SanitizedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SanitizedError")
            .field(&self.message)
            .finish()
    }
}

impl std::error::Error for SanitizedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_urls_keep_safe_host_and_route_but_never_credentials() {
        let original = "request failed: https://discord.com/api/v10/webhooks/123456/private-hook-secret?wait=true&token=query-secret";
        let safe = redact_sensitive(original);
        assert!(safe.contains("https://discord.com/api/v10/[redacted]"));
        assert!(!safe.contains("webhooks"));
        assert!(!safe.contains("private-hook-secret"));
        assert!(!safe.contains("query-secret"));
        assert!(!safe.contains("wait=true"));
    }

    #[test]
    fn matrix_urls_userinfo_fragments_and_authorization_are_private() {
        let original = "https://alice:private-password@matrix.example/_matrix/client/v3/sync?access_token=matrix-secret#fragment Authorization: Bearer discord-bot-secret";
        let safe = redact_sensitive(original);
        assert!(safe.contains("https://matrix.example/_matrix/client/v3/sync"));
        for secret in [
            "alice",
            "private-password",
            "matrix-secret",
            "fragment",
            "discord-bot-secret",
        ] {
            assert!(!safe.contains(secret), "leaked {secret}: {safe}");
        }
    }

    #[test]
    fn structured_and_case_insensitive_secret_fields_are_redacted() {
        let original = r#"{"access_token":"matrix-secret","password":"login-secret","client_secret":"oauth-secret"} authorization=Bot bot-secret token=plain-secret"#;
        let safe = redact_sensitive(original);
        for secret in [
            "matrix-secret",
            "login-secret",
            "oauth-secret",
            "bot-secret",
            "plain-secret",
        ] {
            assert!(!safe.contains(secret), "leaked {secret}: {safe}");
        }
        assert!(safe.contains(REDACTED));
    }

    #[test]
    fn error_display_and_debug_redact_nested_service_credentials() {
        let discord = crate::discord::DiscordError::Http {
            status: 401,
            message:
                r#"{"access_token":"response-secret","message":"authorization=Bearer body-secret"}"#
                    .to_string(),
        };
        let chat = crate::error::ChatError::MessageSend {
            service_name: "Matrix".to_string(),
            reason:
                "https://matrix.example/_matrix/client/v3/sync?access_token=matrix-error-secret"
                    .to_string(),
        };
        let service = crate::error::ServiceError::Runtime {
            reason: "password=service-error-secret".to_string(),
        };
        let top_level = crate::error::ChatstronomyError::Generic {
            message: "https://discord.com/api/webhooks/123/top-level-webhook-secret password=top-level-password-secret"
                .to_string(),
        };
        for rendered in [
            discord.to_string(),
            format!("{discord:?}"),
            chat.to_string(),
            format!("{chat:?}"),
            service.to_string(),
            format!("{service:?}"),
            top_level.to_string(),
            format!("{top_level:?}"),
        ] {
            for secret in [
                "response-secret",
                "body-secret",
                "matrix-error-secret",
                "service-error-secret",
                "top-level-webhook-secret",
                "top-level-password-secret",
            ] {
                assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
            }
        }
    }

    #[test]
    fn webhook_client_debug_hides_entire_private_endpoint() {
        let client = crate::discord::DiscordWebhook::new(
            "https://discord.com/api/webhooks/123/private-webhook-token".to_string(),
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("private-webhook-token"));
        assert!(!debug.contains("api/webhooks"));
        assert!(debug.contains("discord.com"));
    }

    #[tokio::test]
    async fn genuine_request_error_hides_secrets_in_display_debug_and_source() {
        use std::error::Error;

        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get("http://127.0.0.1:1/api/webhooks/42/real-webhook-secret?access_token=real-query-secret")
            .send()
            .await
            .unwrap_err();
        let safe = SafeHttpError::from(error);
        let display = safe.to_string();
        let debug = format!("{safe:?}");
        assert!(safe.source().is_none(), "unsafe request chain was exposed");
        for rendered in [&display, &debug] {
            assert!(!rendered.contains("real-webhook-secret"), "{rendered}");
            assert!(!rendered.contains("real-query-secret"), "{rendered}");
        }
        assert!(display.contains("127.0.0.1"));
        assert!(display.contains("/api/[redacted]"));
        assert!(!display.contains("webhooks"));
    }
}
