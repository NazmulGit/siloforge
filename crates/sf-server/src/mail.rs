//! v2.4 outgoing mail: SMTP via `lettre` (rustls + ring, no OpenSSL) when Settings → Platform has
//! an SMTP host, else (or when sending fails) the message lands in `mail_outbox` so platform admins
//! can deliver the link by hand (Admin → Email). Never logs bodies or links.

use std::time::Duration;

use lettre::message::{Mailbox, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use rusqlite::params;
use serde_json::json;
use sf_store::err;

use crate::platform::{Platform, Smtp};
use crate::{ApiResult, AppState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Smtp,
    Manual,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Smtp => "smtp",
            Delivery::Manual => "manual",
        }
    }
}

pub struct Mail<'a> {
    pub to: &'a str,
    pub subject: &'a str,
    pub body: String,
    /// `verify_email` | `reset_password` | `change_email` | `invite` | `test` | ...
    pub kind: &'a str,
    /// The action link (kept in the outbox only when the mail was not sent).
    pub link: Option<String>,
}

const SEND_TIMEOUT: Duration = Duration::from_secs(20);

fn transport(smtp: &Smtp, password: &str) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let host = smtp.host.trim();
    let mut b = match smtp.tls.as_str() {
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        mode => {
            let params = TlsParameters::new(host.to_string()).map_err(|e| e.to_string())?;
            let tls = if mode == "tls" {
                Tls::Wrapper(params)
            } else {
                Tls::Required(params)
            };
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host).tls(tls)
        }
    };
    b = b.port(smtp.port).timeout(Some(SEND_TIMEOUT));
    if !smtp.username.is_empty() {
        b = b.credentials(Credentials::new(
            smtp.username.clone(),
            password.to_string(),
        ));
    }
    Ok(b.build())
}

/// Sends one message through SMTP (`Err` = human-readable reason, never secrets).
pub async fn send_smtp(st: &AppState, p: &Platform, m: &Mail<'_>) -> Result<(), String> {
    let smtp = &p.smtp;
    let password = if smtp.password_enc.is_empty() {
        String::new()
    } else {
        sf_core::crypto::decrypt_secret(&st.secret_key, &smtp.password_enc)
            .map_err(|_| "stored SMTP password cannot be decrypted".to_string())?
    };
    let from_name = if smtp.from_name.trim().is_empty() {
        p.branding.app_name.clone()
    } else {
        smtp.from_name.clone()
    };
    let from: Mailbox = format!(
        "{} <{}>",
        from_name.replace(['"', '<', '>'], ""),
        smtp.from_email.trim()
    )
    .parse()
    .map_err(|e| format!("invalid from address: {e}"))?;
    let to: Mailbox =
        m.to.parse()
            .map_err(|e| format!("invalid recipient: {e}"))?;
    let msg = Message::builder()
        .from(from)
        .to(to)
        .subject(m.subject)
        .header(ContentType::TEXT_PLAIN)
        .body(m.body.clone())
        .map_err(|e| format!("cannot build message: {e}"))?;
    let tr = transport(smtp, &password)?;
    match tokio::time::timeout(SEND_TIMEOUT, tr.send(msg)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("smtp: {e}")),
        Err(_) => Err("smtp: timed out".into()),
    }
}

/// Sends (or queues for manual delivery) and records the outcome in `mail_outbox`.
pub async fn send(st: &AppState, m: Mail<'_>) -> ApiResult<Delivery> {
    let p = crate::platform::get(st).await;
    let (delivery, error) = if p.smtp.configured() {
        match send_smtp(st, &p, &m).await {
            Ok(()) => (Delivery::Smtp, None),
            Err(e) => {
                tracing::warn!(kind = m.kind, error = %e, "mail send failed; kept for manual delivery");
                (Delivery::Manual, Some(e))
            }
        }
    } else {
        (Delivery::Manual, None)
    };
    let sent = delivery == Delivery::Smtp;
    let (to, subject, kind) = (m.to.to_string(), m.subject.to_string(), m.kind.to_string());
    // Bodies of sent mail carry live links: keep only the subject then.
    let body = if sent { String::new() } else { m.body.clone() };
    let link = if sent { None } else { m.link.clone() };
    let err2 = error.clone();
    st.db
        .write(move |t| {
            t.execute(
                "INSERT INTO mail_outbox(to_email, subject, body, kind, link, sent_at, error) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    to,
                    subject,
                    body,
                    kind,
                    link,
                    sent.then(sf_core::now_rfc3339),
                    err2
                ],
            )
            .map_err(err)?;
            // bounded: keep the newest 2000 rows
            t.execute(
                "DELETE FROM mail_outbox WHERE id NOT IN (SELECT id FROM mail_outbox ORDER BY id DESC LIMIT 2000)",
                [],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    crate::eventlog::record_system(
        st,
        &format!("mail {}", m.kind),
        error.is_none(),
        json!({ "delivery": delivery.as_str(), "error": error }),
    );
    Ok(delivery)
}

/// Absolute link for a UI path (`/reset-password?token=...`): public_url, else the request host.
pub fn link(p: &Platform, host_fallback: Option<&str>, path: &str) -> String {
    let base = p.public_url();
    let base = if base.is_empty() {
        match host_fallback {
            Some(h) if !h.is_empty() => format!("http://{h}"),
            _ => "http://127.0.0.1:8601".to_string(),
        }
    } else {
        base
    };
    format!("{base}{path}")
}

/// Request host for links when no public_url is set. Only IP literals / `localhost` are trusted
/// (a forged `Host` must never steer a password-reset link to another domain).
pub fn request_host(h: &axum::http::HeaderMap) -> Option<String> {
    if !crate::v1::auth::bootstrap_host_ok(h) {
        return None;
    }
    let v = h.get(axum::http::header::HOST)?.to_str().ok()?.trim();
    (!v.is_empty()
        && v.len() <= 255
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')))
    .then(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_prefer_public_url() {
        let mut p = Platform::default();
        assert_eq!(
            link(&p, Some("127.0.0.1:8601"), "/x"),
            "http://127.0.0.1:8601/x"
        );
        p.deployment.public_url = "https://links.example.com/".into();
        assert_eq!(link(&p, Some("evil"), "/x"), "https://links.example.com/x");
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::HOST, "a b".parse().unwrap());
        assert!(request_host(&h).is_none());
    }
}
