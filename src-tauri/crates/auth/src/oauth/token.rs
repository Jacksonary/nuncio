//! OAuth token payloads, expiry handling and (unsigned) ID-token decoding.

use std::collections::HashMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use nuncio_core::error::AppError;
use nuncio_core::Result;

/// Refresh this many seconds before the server-side expiry.
const EXPIRY_SKEW_SECONDS: i64 = 60;

/// A cached access token with an absolute expiry instant.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub value: String,
    pub expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("value", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl AccessToken {
    pub fn new(value: impl Into<String>, expires_at: DateTime<Utc>) -> Self {
        Self {
            value: value.into(),
            expires_at,
        }
    }

    /// Valid until the cached expiry (already includes the refresh margin).
    pub fn is_valid(&self, now: DateTime<Utc>) -> bool {
        now < self.expires_at
    }
}

/// Successful response from Google/Microsoft token endpoints.
///
/// `access_token` is defaulted so error payloads can be inspected by the same
/// parser without a serde distinction.
#[derive(Clone, Default, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field(
                "access_token",
                &(!self.access_token.is_empty()).then_some("<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("scope", &self.scope)
            .field("token_type", &self.token_type)
            .finish()
    }
}

impl TokenResponse {
    /// Convert the response into an [`AccessToken`].  Missing `expires_in`
    /// defaults to one hour (both providers always send it in practice).
    pub fn access_token(&self, now: DateTime<Utc>) -> AccessToken {
        let seconds = self.expires_in.unwrap_or(3600).clamp(0, 86_400 * 30);
        AccessToken::new(self.access_token.clone(), now + Duration::seconds(seconds))
    }

    /// Token as it should be cached: expiry minus the 60-second refresh margin.
    pub fn cache_token(&self, now: DateTime<Utc>) -> AccessToken {
        let mut token = self.access_token(now);
        token.expires_at -= Duration::seconds(EXPIRY_SKEW_SECONDS);
        token
    }

    pub fn refresh_token_value(&self) -> Option<&str> {
        self.refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
}

/// RFC 6749 error (`{"error": "...", "error_description": "..."}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenError {
    pub code: String,
    pub description: String,
}

impl TokenError {
    pub fn new(code: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            description: description.into(),
        }
    }

    /// The refresh token is no longer accepted -> the account needs a re-login.
    pub fn is_invalid_grant(&self) -> bool {
        self.code.eq_ignore_ascii_case("invalid_grant")
    }

    pub fn message(&self) -> String {
        if self.description.is_empty() {
            self.code.clone()
        } else {
            format!("{}: {}", self.code, self.description)
        }
    }

    pub fn into_app_error(self, context: &str) -> AppError {
        AppError::auth(format!("{context}: {}", self.message()))
    }
}

#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Decode a token endpoint response.  Never includes the raw body (which may
/// contain a token) in the produced error.
pub fn decode_token_response(
    status: u16,
    body: &str,
) -> std::result::Result<TokenResponse, TokenError> {
    let parsed: Option<TokenResponse> = serde_json::from_str(body).ok();
    if let Some(response) = parsed {
        if !response.access_token.trim().is_empty() {
            return Ok(response);
        }
    }

    if let Ok(error) = serde_json::from_str::<ErrorBody>(body) {
        if let Some(code) = error.error.filter(|code| !code.trim().is_empty()) {
            let description = error
                .error_description
                .unwrap_or_default()
                .trim()
                .to_string();
            return Err(TokenError::new(code.trim(), description));
        }
    }

    let code = if (200..300).contains(&status) {
        "invalid_response".to_string()
    } else {
        format!("http_{status}")
    };
    let description = if (200..300).contains(&status) {
        "token endpoint did not return an access_token".to_string()
    } else {
        format!("token endpoint returned HTTP {status}")
    };
    Err(TokenError::new(code, description))
}

/// Claims we care about from an OpenID Connect ID token.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct IdTokenClaims {
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub upn: Option<String>,
    #[serde(default)]
    pub unique_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub given_name: Option<String>,
}

/// Decode the payload of a JWT **without** verifying the signature.
///
/// This is safe here because the token came directly from the provider's token
/// endpoint over TLS in response to our own authorization-code exchange (§ the
/// task explicitly asks for unsigned claim decoding).
pub fn decode_id_token(id_token: &str) -> Result<IdTokenClaims> {
    let mut parts = id_token.split('.');
    let _header = parts
        .next()
        .ok_or_else(|| AppError::auth("ID token is not a JWT"))?;
    let payload = parts
        .next()
        .ok_or_else(|| AppError::auth("ID token is not a JWT"))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|error| AppError::auth(format!("ID token payload is not base64url: {error}")))?;
    serde_json::from_slice(&decoded)
        .map_err(|error| AppError::auth(format!("ID token payload is not JSON: {error}")))
}

/// User profile returned by Google's OpenID userinfo endpoint.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GoogleUserInfo {
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub given_name: Option<String>,
}

/// Result of resolving the signed-in user's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthIdentity {
    pub email: String,
    pub display_name: String,
}

fn is_email_like(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.contains('@') && !value.starts_with('@')
}

fn display_from_email(email: &str) -> String {
    email
        .split('@')
        .next()
        .filter(|local| !local.is_empty())
        .unwrap_or(email)
        .to_string()
}

/// Derive an identity from Microsoft ID-token claims.
pub fn microsoft_identity(
    claims: &IdTokenClaims,
    fallback_email: Option<&str>,
) -> Result<OAuthIdentity> {
    let email = [
        claims.preferred_username.as_deref(),
        claims.email.as_deref(),
        claims.upn.as_deref(),
        claims.unique_name.as_deref(),
        fallback_email,
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| is_email_like(value))
    .ok_or_else(|| {
        AppError::auth(
            "Microsoft did not return an e-mail address in the ID token; enter it manually",
        )
    })?
    .to_string();

    let display_name = claims
        .name
        .as_deref()
        .or(claims.given_name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| display_from_email(&email));

    Ok(OAuthIdentity {
        email,
        display_name,
    })
}

/// Derive an identity from Google's userinfo response.
pub fn google_identity(info: &GoogleUserInfo) -> Result<OAuthIdentity> {
    let email = info
        .email
        .as_deref()
        .map(str::trim)
        .filter(|value| is_email_like(value))
        .ok_or_else(|| AppError::auth("Google did not return an e-mail address"))?
        .to_string();

    let display_name = info
        .name
        .as_deref()
        .or(info.given_name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| display_from_email(&email));

    Ok(OAuthIdentity {
        email,
        display_name,
    })
}

/// Tokens keyed by storage suffix, produced by an interactive login and held
/// until `attach_session` persists them.
#[derive(Clone)]
pub struct PendingTokens {
    pub access: AccessToken,
    pub refresh: String,
    pub graph_access: Option<AccessToken>,
    pub graph_refresh: Option<String>,
}

impl std::fmt::Debug for PendingTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingTokens")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field(
                "graph_access",
                &self.graph_access.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "graph_refresh",
                &self.graph_refresh.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl PendingTokens {
    /// Map of refresh-token storage suffix -> value.
    pub fn refresh_tokens(&self) -> HashMap<&'static str, &str> {
        let mut map = HashMap::new();
        map.insert("refresh_token", self.refresh.as_str());
        if let Some(refresh) = self.graph_refresh.as_deref() {
            map.insert("refresh_token:graph", refresh);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_and_applies_expiry_skew() {
        let body = r#"{"access_token":"tok","expires_in":3600,"refresh_token":"r1","token_type":"Bearer"}"#;
        let response = decode_token_response(200, body).expect("parse");
        assert_eq!(response.access_token, "tok");
        assert_eq!(response.refresh_token_value(), Some("r1"));

        let now = Utc::now();
        let token = response.cache_token(now);
        assert!(token.is_valid(now));
        assert!(token.is_valid(now + Duration::seconds(3539)));
        assert!(!token.is_valid(now + Duration::seconds(3541)));
        assert!(!token.is_valid(now + Duration::seconds(3600)));
        assert_eq!(
            response.access_token(now).expires_at,
            now + Duration::seconds(3600)
        );
    }

    #[test]
    fn missing_expires_in_defaults_to_one_hour() {
        let response = decode_token_response(200, r#"{"access_token":"tok"}"#).expect("parse");
        let now = Utc::now();
        assert_eq!(
            response.access_token(now).expires_at,
            now + Duration::seconds(3600)
        );
    }

    #[test]
    fn parses_error_payload() {
        let error = decode_token_response(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )
        .expect_err("must fail");
        assert!(error.is_invalid_grant());
        assert!(error.message().contains("invalid_grant"));
        assert!(error.message().contains("revoked"));
    }

    #[test]
    fn non_json_error_does_not_leak_body() {
        let error = decode_token_response(502, "<html>bad gateway</html>").expect_err("must fail");
        assert_eq!(error.code, "http_502");
        assert!(!error.message().contains("html"));
    }

    #[test]
    fn success_without_access_token_is_rejected() {
        let error = decode_token_response(200, r#"{"expires_in":3600}"#).expect_err("must fail");
        assert_eq!(error.code, "invalid_response");
    }

    fn jwt(payload: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(payload);
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn decodes_id_token_claims() {
        let token = jwt(
            r#"{"preferred_username":"User@Example.com","name":"Example User","email":"other@example.com"}"#,
        );
        let claims = decode_id_token(&token).expect("decode");
        assert_eq!(
            claims.preferred_username.as_deref(),
            Some("User@Example.com")
        );
        let identity = microsoft_identity(&claims, None).expect("identity");
        assert_eq!(identity.email, "User@Example.com");
        assert_eq!(identity.display_name, "Example User");
    }

    #[test]
    fn microsoft_falls_back_to_email_claim_and_local_part() {
        let claims = IdTokenClaims {
            preferred_username: Some("00000000-0000-0000-0000-000000000000".into()),
            email: Some("person@outlook.com".into()),
            ..Default::default()
        };
        let identity = microsoft_identity(&claims, None).expect("identity");
        assert_eq!(identity.email, "person@outlook.com");
        assert_eq!(identity.display_name, "person");

        let claims = IdTokenClaims {
            preferred_username: Some("42".into()),
            ..Default::default()
        };
        assert!(microsoft_identity(&claims, None).is_err());
    }

    #[test]
    fn decodes_malformed_id_token_as_error() {
        assert!(decode_id_token("not-a-jwt").is_err());
        assert!(decode_id_token("a.%%%%%.c").is_err());
    }

    #[test]
    fn google_identity_prefers_name() {
        let identity = google_identity(&GoogleUserInfo {
            email: Some("g@example.com".into()),
            name: Some("Gmail User".into()),
            ..Default::default()
        })
        .expect("identity");
        assert_eq!(identity.email, "g@example.com");
        assert_eq!(identity.display_name, "Gmail User");
    }
}
