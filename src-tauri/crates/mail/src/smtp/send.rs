//! SMTP submission using lettre (rustls + ring).
//!
//! Security mapping:
//! * `tls`      -> implicit TLS (usually port 465)
//! * `starttls` -> required STARTTLS (usually port 587)
//! * `none`     -> plain SMTP (port 25; only for local relays)

use std::time::Duration;

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::Error as SmtpError;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use nuncio_core::models::{Address, Security, ServerConfig};
use nuncio_core::traits::MailAuth;
use nuncio_core::{AppError, CoreContext, Result};
use tracing::{debug, info, warn};

use crate::util::ensure_crypto_provider;

const SMTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard ceiling for one message, covering connect, auth and DATA together.
/// Must stay well below `SENDING_LEASE` in `smtp::outbox` (10 minutes) so a
/// lease can only expire after the owning task has already given up.
pub const SEND_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Send raw RFC822 bytes through an SMTP server.
pub async fn send_raw(
    config: &ServerConfig,
    auth: &MailAuth,
    from: Option<&Address>,
    recipients: &[Address],
    bytes: &[u8],
) -> Result<()> {
    if recipients.is_empty() {
        return Err(AppError::invalid_input("message has no recipients"));
    }
    if config.security == Security::None && !crate::util::is_loopback_host(&config.host) {
        return Err(AppError::invalid_input(
            "SMTP without TLS is only allowed for a local (loopback) relay; \
             choose TLS or STARTTLS for this server",
        ));
    }
    ensure_crypto_provider();

    let tls_parameters = match config.security {
        Security::Tls | Security::Starttls => Some(
            TlsParameters::new_rustls(config.host.clone())
                .map_err(|error| AppError::network(format!("SMTP TLS setup failed: {error}")))?,
        ),
        Security::None => None,
    };
    let tls = match config.security {
        Security::Tls => {
            Tls::Wrapper(tls_parameters.clone().ok_or_else(|| {
                AppError::internal("SMTP TLS parameters missing for implicit TLS")
            })?)
        }
        Security::Starttls => Tls::Required(
            tls_parameters
                .ok_or_else(|| AppError::internal("SMTP TLS parameters missing for STARTTLS"))?,
        ),
        Security::None => Tls::None,
    };

    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
        .port(config.port)
        .tls(tls)
        .timeout(Some(SMTP_TIMEOUT));
    match auth {
        MailAuth::Password { username, password } => {
            builder = builder
                .credentials(Credentials::new(username.clone(), password.clone()))
                .authentication(vec![Mechanism::Plain, Mechanism::Login]);
        }
        MailAuth::XOAuth2 {
            username,
            access_token,
        } => {
            builder = builder
                .credentials(Credentials::new(username.clone(), access_token.clone()))
                .authentication(vec![Mechanism::Xoauth2]);
        }
    }
    let transport: AsyncSmtpTransport<Tokio1Executor> = builder.build();

    let from = match from {
        Some(address) => Some(parse_mailbox(address)?),
        None => None,
    };
    let recipients: Vec<Mailbox> = recipients
        .iter()
        .map(parse_mailbox)
        .collect::<Result<Vec<_>>>()?;
    let envelope = lettre::address::Envelope::new(
        from.map(|mailbox| mailbox.email),
        recipients
            .iter()
            .map(|mailbox| mailbox.email.clone())
            .collect(),
    )
    .map_err(|error| AppError::invalid_input(format!("invalid SMTP envelope: {error}")))?;

    debug!(
        host = %config.host,
        port = config.port,
        recipients = recipients.len(),
        "sending message over SMTP"
    );
    // RFC 5322 privacy: Bcc recipients belong in the envelope only. `lettre`
    // sends the bytes verbatim, so drop the header (and its folded
    // continuation lines) before DATA.
    let payload = strip_bcc_header(bytes);
    // `timeout()` is a per-operation socket timeout; a server can keep answering
    // just often enough to stretch the whole conversation indefinitely. The hard
    // deadline below is what makes the outbox lease sound: a send either finishes
    // inside `SEND_DEADLINE` or is cancelled and reported as failed.
    let send = transport.send_raw(&envelope, &payload);
    match tokio::time::timeout(SEND_DEADLINE, send).await {
        Ok(result) => {
            result.map_err(smtp_error)?;
        }
        Err(_) => {
            return Err(AppError::network(format!(
                "SMTP send exceeded {} seconds",
                SEND_DEADLINE.as_secs()
            )));
        }
    }
    info!(host = %config.host, "message sent over SMTP");
    Ok(())
}

fn parse_mailbox(address: &Address) -> Result<Mailbox> {
    let email = address.email.parse().map_err(|error| {
        AppError::invalid_input(format!("invalid e-mail address {}: {error}", address.email))
    })?;
    Ok(Mailbox::new(address.name.clone(), email))
}

fn smtp_error(error: SmtpError) -> AppError {
    let message = error.to_string();
    if error.is_permanent() {
        let lower = message.to_ascii_lowercase();
        if lower.contains("auth") || lower.contains("credential") || lower.contains("login") {
            AppError::auth(format!("SMTP authentication failed: {message}"))
        } else {
            AppError::invalid_input(format!("SMTP permanently rejected the message: {message}"))
        }
    } else if error.is_tls() {
        AppError::network(format!("SMTP TLS error: {message}"))
    } else {
        AppError::network(format!("SMTP error: {message}"))
    }
}

/// Remove `Bcc:` / `Resent-Bcc:` headers (with folded continuation lines)
/// from an RFC822 message. The envelope keeps the recipients; the DATA payload
/// must not expose them (RFC 5322 §3.6.3).
pub fn strip_bcc_header(bytes: &[u8]) -> Vec<u8> {
    let boundary = header_boundary(bytes);
    let (header_end, body_start) = match boundary {
        Some(value) => value,
        None => return bytes.to_vec(),
    };
    let headers = &bytes[..header_end];
    let body = &bytes[body_start..];
    let mut out = Vec::with_capacity(bytes.len());
    let mut start = 0usize;
    let mut skipping = false;
    let mut index = 0usize;
    while index <= headers.len() {
        let at_end = index == headers.len();
        if at_end || headers[index] == b'\n' {
            let line = &headers[start..index];
            let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
            let is_continuation = matches!(trimmed.first(), Some(b' ' | b'\t'));
            if skipping && is_continuation {
                // Part of a folded Bcc header.
            } else {
                skipping = is_bcc_header(trimmed);
                if !skipping {
                    out.extend_from_slice(line);
                    if !at_end {
                        out.push(b'\n');
                    }
                }
            }
            start = index + 1;
        }
        index += 1;
    }
    // Preserve the blank separator line and the body untouched.
    out.extend_from_slice(body);
    out
}

fn header_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, position));
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, position));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn is_bcc_header(line: &[u8]) -> bool {
    const NAMES: [&[u8]; 2] = [b"bcc:", b"resent-bcc:"];
    NAMES
        .iter()
        .any(|name| line.len() >= name.len() && line[..name.len()].eq_ignore_ascii_case(name))
}

/// Parse a built message to discover its sender and recipients.
pub fn envelope_from_message(
    bytes: &[u8],
    fallback_from: &Address,
) -> Result<(Address, Vec<Address>)> {
    let parsed = match nuncio_mime::parse(bytes) {
        Ok(parsed) => parsed,
        Err(error) => {
            warn!(%error, "cannot parse outgoing message; falling back to database recipients");
            return Err(error);
        }
    };
    let from = parsed
        .headers
        .from
        .clone()
        .unwrap_or_else(|| fallback_from.clone());
    let mut recipients = parsed.headers.to.clone();
    recipients.extend(parsed.headers.cc.clone());
    recipients.extend(parsed.headers.bcc.clone());
    let recipients = nuncio_mime::dedupe_addresses(recipients);
    if recipients.is_empty() {
        return Err(AppError::invalid_input("message has no recipients"));
    }
    Ok((from, recipients))
}

/// Build the SMTP envelope from an explicit recipient list when one is
/// available, otherwise fall back to the message headers.
pub fn envelope_for(
    bytes: &[u8],
    fallback_from: &Address,
    explicit_recipients: Option<&[Address]>,
) -> Result<(Address, Vec<Address>)> {
    match explicit_recipients.filter(|recipients| !recipients.is_empty()) {
        Some(recipients) => {
            let from = nuncio_mime::parse(bytes)
                .ok()
                .and_then(|parsed| parsed.headers.from)
                .unwrap_or_else(|| fallback_from.clone());
            let recipients = nuncio_mime::dedupe_addresses(recipients.to_vec());
            if recipients.is_empty() {
                return Err(AppError::invalid_input("message has no recipients"));
            }
            Ok((from, recipients))
        }
        None => envelope_from_message(bytes, fallback_from),
    }
}

/// Send a message for an account using its SMTP configuration and credentials.
///
/// `recipients` is the authoritative envelope recipient list. The outbox passes
/// the explicit To+Cc+Bcc list because `build_message` deliberately omits the
/// Bcc header from the transmitted bytes, so it cannot be recovered from the
/// headers alone. `None` falls back to parsing the message headers (iTIP and
/// other raw senders keep that behaviour).
pub async fn send_account_message(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    bytes: &[u8],
    recipients: Option<&[Address]>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + SEND_DEADLINE;
    send_account_message_within(ctx, account, bytes, recipients, deadline).await
}

/// One outbox claim, one deadline.
///
/// `deadline` is shared by every attempt this claim makes, so an auth retry gets
/// the *remaining* time rather than a fresh `SEND_DEADLINE`. Without that a
/// single message could transmit for twice the outbox lease and still be
/// reclaimed — which is how a duplicate delivery happens.
pub async fn send_account_message_within(
    ctx: &CoreContext,
    account: &nuncio_core::models::Account,
    bytes: &[u8],
    recipients: Option<&[Address]>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let config = account
        .smtp
        .as_ref()
        .ok_or_else(|| AppError::unsupported("account has no SMTP server configured"))?;
    let auth = match tokio::time::timeout_at(deadline, ctx.creds.mail_auth(account)).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(AppError::network(
                "SMTP send deadline reached while fetching credentials",
            ))
        }
    };
    let (from, recipients) = envelope_for(bytes, &account.address(), recipients)?;
    let first = match tokio::time::timeout_at(
        deadline,
        send_raw(config, &auth, Some(&from), &recipients, bytes),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            return Err(AppError::network(format!(
                "SMTP send exceeded {} seconds",
                SEND_DEADLINE.as_secs()
            )))
        }
    };
    match first {
        Ok(()) => Ok(()),
        Err(error) if error.code() == "auth" || error.code() == "network" => {
            // The credential refresh is part of the same claim budget: a hung
            // token endpoint must not outlive the lease either.
            let refreshed = match tokio::time::timeout_at(deadline, async {
                if error.code() == "auth" {
                    ctx.creds
                        .report_auth_failure(&account.id, nuncio_core::traits::TokenResource::Mail)
                        .await;
                }
                ctx.creds.mail_auth(account).await
            })
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    return Err(AppError::network(
                        "SMTP send deadline reached while refreshing credentials",
                    ))
                }
            };
            match tokio::time::timeout_at(
                deadline,
                send_raw(config, &refreshed, Some(&from), &recipients, bytes),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(AppError::network(
                    "SMTP send exceeded its deadline after re-authenticating",
                )),
            }
        }
        Err(error) => Err(error),
    }
}

/// Connectivity probe used by `account_test`.
pub async fn test_smtp(config: &ServerConfig, auth: &MailAuth) -> Result<()> {
    if config.security == Security::None && !crate::util::is_loopback_host(&config.host) {
        return Err(AppError::invalid_input(
            "SMTP without TLS is only allowed for a local (loopback) relay; \
             choose TLS or STARTTLS for this server",
        ));
    }
    ensure_crypto_provider();
    let tls_parameters = match config.security {
        Security::Tls | Security::Starttls => Some(
            TlsParameters::new_rustls(config.host.clone())
                .map_err(|error| AppError::network(format!("SMTP TLS setup failed: {error}")))?,
        ),
        Security::None => None,
    };
    let tls = match config.security {
        Security::Tls => Tls::Wrapper(
            tls_parameters.ok_or_else(|| AppError::internal("missing TLS parameters"))?,
        ),
        Security::Starttls => Tls::Required(
            tls_parameters.ok_or_else(|| AppError::internal("missing TLS parameters"))?,
        ),
        Security::None => Tls::None,
    };
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
        .port(config.port)
        .tls(tls)
        .timeout(Some(SMTP_TIMEOUT));
    match auth {
        MailAuth::Password { username, password } => {
            builder = builder
                .credentials(Credentials::new(username.clone(), password.clone()))
                .authentication(vec![Mechanism::Plain, Mechanism::Login]);
        }
        MailAuth::XOAuth2 {
            username,
            access_token,
        } => {
            builder = builder
                .credentials(Credentials::new(username.clone(), access_token.clone()))
                .authentication(vec![Mechanism::Xoauth2]);
        }
    }
    let transport: AsyncSmtpTransport<Tokio1Executor> = builder.build();
    transport.test_connection().await.map_err(smtp_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_envelope_from_headers() {
        let raw = concat!(
            "From: Alice <alice@example.com>\r\n",
            "To: Bob <bob@example.com>\r\n",
            "Cc: carol@example.com\r\n",
            "Bcc: dave@example.com\r\n",
            "Subject: hi\r\n",
            "\r\n",
            "body\r\n"
        );
        let (from, recipients) =
            envelope_from_message(raw.as_bytes(), &Address::new("fallback@example.com"))
                .expect("envelope");
        assert_eq!(from.email, "alice@example.com");
        assert_eq!(recipients.len(), 3);
    }

    #[test]
    fn strips_bcc_and_folded_continuations_from_payload() {
        let raw = concat!(
            "From: Alice <alice@example.com>\r\n",
            "To: Bob <bob@example.com>\r\n",
            "Bcc: dave@example.com,\r\n",
            " eve@example.com\r\n",
            "Resent-Bcc: frank@example.com\r\n",
            "Subject: hi\r\n",
            "\r\n",
            "body mentions Bcc: not-a-header\r\n"
        );
        let (_, recipients) =
            envelope_from_message(raw.as_bytes(), &Address::new("fallback@example.com"))
                .expect("envelope");
        assert_eq!(recipients.len(), 3);

        let stripped = strip_bcc_header(raw.as_bytes());
        let text = String::from_utf8(stripped).expect("utf8");
        let headers = text.split("\r\n\r\n").next().unwrap_or_default();
        assert!(!headers.to_ascii_lowercase().contains("bcc:"));
        assert!(!headers.to_ascii_lowercase().contains("eve@example.com"));
        assert!(headers.contains("To: Bob <bob@example.com>"));
        assert!(headers.contains("Subject: hi"));
        assert!(text.contains("body mentions Bcc: not-a-header"));
    }

    #[test]
    fn strip_bcc_handles_messages_without_headers_or_bcc() {
        assert_eq!(strip_bcc_header(b"body only"), b"body only");
        let plain = b"To: a@example.com\nSubject: x\n\nbody\n";
        assert_eq!(strip_bcc_header(plain), plain);
    }

    #[test]
    fn explicit_recipients_win_over_missing_bcc_header() {
        // `build_message` strips Bcc, so the envelope must come from the
        // outbox row instead of the headers.
        let raw = concat!(
            "From: Alice <alice@example.com>\r\n",
            "To: Bob <bob@example.com>\r\n",
            "Subject: hi\r\n",
            "\r\n",
            "body\r\n"
        );
        let explicit = vec![
            Address::new("bob@example.com"),
            Address::new("dave@example.com"),
        ];
        let (from, recipients) = envelope_for(
            raw.as_bytes(),
            &Address::new("fallback@example.com"),
            Some(&explicit),
        )
        .expect("envelope");
        assert_eq!(from.email, "alice@example.com");
        assert_eq!(recipients.len(), 2);
        assert!(recipients.iter().any(|a| a.email == "dave@example.com"));
    }

    #[test]
    fn rejects_plaintext_authentication_to_remote_hosts() {
        let config = ServerConfig {
            host: "mail.example.com".into(),
            port: 25,
            security: Security::None,
            username: "user".into(),
        };
        let auth = MailAuth::Password {
            username: "user".into(),
            password: "secret".into(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(send_raw(
                &config,
                &auth,
                None,
                &[Address::new("a@example.com")],
                b"raw",
            ))
            .expect_err("plaintext remote SMTP must be rejected");
        assert_eq!(error.code(), "invalidInput");
    }
}
