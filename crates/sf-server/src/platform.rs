//! v2.4 platform settings (registration, SMTP, branding, deployment, billing), stored in the DB
//! (kv `platform_settings`) so they apply live and travel with backups. The SMTP password is
//! encrypted with the instance secret key and never returned (`"***"` / `""`).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sf_store::err;

use crate::{ApiError, ApiResult, AppState};

pub const KV_KEY: &str = "platform_settings";
pub const MODES: &[&str] = &["closed", "invite_only", "open"];
pub const TLS_MODES: &[&str] = &["starttls", "tls", "none"];
pub const BILLING: &[&str] = &["none", "manual", "stripe"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Registration {
    /// `None` until saved: then legacy `auth.allow_registration` decides (true = open).
    pub mode: Option<String>,
    pub require_email_verification: bool,
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
    pub rate_limit_per_hour: u32,
    pub default_plan_id: String,
}

impl Default for Registration {
    fn default() -> Self {
        Self {
            mode: None,
            require_email_verification: false,
            allowed_domains: Vec::new(),
            denied_domains: Vec::new(),
            rate_limit_per_hour: 5,
            default_plan_id: "free".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    pub tls: String,
    pub username: String,
    /// Encrypted (`v1:` blob) in storage.
    pub password_enc: String,
    pub from_email: String,
    pub from_name: String,
}

impl Default for Smtp {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 587,
            tls: "starttls".into(),
            username: String::new(),
            password_enc: String::new(),
            from_email: String::new(),
            from_name: "SiloForge".into(),
        }
    }
}

impl Smtp {
    pub fn configured(&self) -> bool {
        !self.host.trim().is_empty() && !self.from_email.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Branding {
    pub app_name: String,
    pub logo_url: String,
    pub primary_color: String,
    pub support_email: String,
    pub terms_url: String,
    pub privacy_url: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self {
            app_name: "SiloForge".into(),
            logo_url: String::new(),
            primary_color: String::new(),
            support_email: String::new(),
            terms_url: String::new(),
            privacy_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Deployment {
    pub public_url: String,
    pub trust_proxy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Billing {
    pub provider: String,
}

impl Default for Billing {
    fn default() -> Self {
        Self {
            provider: "manual".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Platform {
    pub registration: Registration,
    pub smtp: Smtp,
    pub branding: Branding,
    pub deployment: Deployment,
    pub billing: Billing,
}

impl Platform {
    /// Effective registration mode.
    pub fn mode(&self, legacy_allow: bool) -> &str {
        match self.registration.mode.as_deref() {
            Some(m) if MODES.contains(&m) => m,
            _ if legacy_allow => "open",
            _ => "closed",
        }
    }

    /// Public URL without a trailing slash (empty = unknown).
    pub fn public_url(&self) -> String {
        self.deployment
            .public_url
            .trim()
            .trim_end_matches('/')
            .to_string()
    }

    /// API view (secrets masked).
    pub fn to_json(&self, legacy_allow: bool) -> Value {
        json!({
            "registration": {
                "mode": self.mode(legacy_allow),
                "require_email_verification": self.registration.require_email_verification,
                "allowed_domains": self.registration.allowed_domains,
                "denied_domains": self.registration.denied_domains,
                "rate_limit_per_hour": self.registration.rate_limit_per_hour,
                "default_plan_id": self.registration.default_plan_id,
            },
            "smtp": {
                "host": self.smtp.host, "port": self.smtp.port, "tls": self.smtp.tls,
                "username": self.smtp.username,
                "password": if self.smtp.password_enc.is_empty() { "" } else { "***" },
                "from_email": self.smtp.from_email, "from_name": self.smtp.from_name,
                "configured": self.smtp.configured(),
            },
            "branding": self.branding,
            "deployment": self.deployment,
            "billing": self.billing,
        })
    }
}

/// Current settings (cached in `AppState`).
pub async fn get(st: &AppState) -> Platform {
    if let Some(p) = st.platform.read().clone() {
        return p;
    }
    let raw: Option<String> = st
        .db
        .read(|c| {
            use rusqlite::OptionalExtension;
            c.query_row("SELECT value FROM kv WHERE key=?1", [KV_KEY], |r| r.get(0))
                .optional()
                .map_err(err)
        })
        .await
        .ok()
        .flatten();
    let p: Platform = raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    *st.platform.write() = Some(p.clone());
    p
}

/// Cached `trust_proxy` without a DB read (false until the settings were loaded once).
pub fn trust_proxy_cached(st: &AppState) -> bool {
    st.platform
        .read()
        .as_ref()
        .is_some_and(|p| p.deployment.trust_proxy)
}

async fn save(st: &AppState, p: &Platform) -> ApiResult<()> {
    let v = serde_json::to_string(p).map_err(|e| ApiError::internal(e.to_string()))?;
    st.db
        .write(move |t| {
            t.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                rusqlite::params![KV_KEY, v, sf_core::now_rfc3339()],
            )
            .map_err(err)?;
            Ok(())
        })
        .await?;
    *st.platform.write() = Some(p.clone());
    Ok(())
}

fn str_field(
    obj: &serde_json::Map<String, Value>,
    k: &str,
    max: usize,
) -> ApiResult<Option<String>> {
    match obj.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.chars().count() > max {
                return Err(ApiError::bad(format!("{k} is too long (max {max})")));
            }
            Ok(Some(s.to_string()))
        }
        Some(Value::Null) => Ok(Some(String::new())),
        Some(_) => Err(ApiError::bad(format!("{k} must be a string"))),
    }
}

fn bool_field(obj: &serde_json::Map<String, Value>, k: &str) -> ApiResult<Option<bool>> {
    match obj.get(k) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ApiError::bad(format!("{k} must be true or false"))),
    }
}

fn url_ok(s: &str) -> bool {
    s.is_empty() || crate::v1::common::is_http_url(s)
}

fn domains(v: &Value, k: &str) -> ApiResult<Vec<String>> {
    let arr = v
        .as_array()
        .ok_or_else(|| ApiError::bad(format!("{k} must be a list of domains")))?;
    if arr.len() > 500 {
        return Err(ApiError::bad(format!("{k}: at most 500 domains")));
    }
    let mut out = Vec::new();
    for d in arr {
        let d = d
            .as_str()
            .ok_or_else(|| ApiError::bad(format!("{k} must be a list of domains")))?
            .trim()
            .trim_start_matches('@')
            .to_ascii_lowercase();
        if d.is_empty() {
            continue;
        }
        if d.len() > 253
            || !d
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return Err(ApiError::bad(format!("{k}: invalid domain {d}")));
        }
        out.push(d);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Validates and merges a PUT body (sections are merged key by key). Returns the changed keys.
pub async fn put(st: &AppState, body: &Value) -> ApiResult<(Platform, Vec<String>)> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad("body must be an object"))?;
    let mut p = get(st).await;
    let before = p.clone();
    for k in obj.keys() {
        if !["registration", "smtp", "branding", "deployment", "billing"].contains(&k.as_str()) {
            return Err(ApiError::bad(format!("unknown section {k}")));
        }
    }
    let sec = |name: &str| -> ApiResult<Option<&serde_json::Map<String, Value>>> {
        match obj.get(name) {
            None => Ok(None),
            Some(Value::Object(m)) => Ok(Some(m)),
            Some(_) => Err(ApiError::bad(format!("{name} must be an object"))),
        }
    };
    if let Some(r) = sec("registration")? {
        if let Some(m) = str_field(r, "mode", 20)? {
            if !MODES.contains(&m.as_str()) {
                return Err(ApiError::bad(
                    "registration.mode must be closed, invite_only or open",
                ));
            }
            p.registration.mode = Some(m);
        }
        if let Some(b) = bool_field(r, "require_email_verification")? {
            p.registration.require_email_verification = b;
        }
        if let Some(v) = r.get("allowed_domains") {
            p.registration.allowed_domains = domains(v, "allowed_domains")?;
        }
        if let Some(v) = r.get("denied_domains") {
            p.registration.denied_domains = domains(v, "denied_domains")?;
        }
        if let Some(v) = r.get("rate_limit_per_hour") {
            let n = v
                .as_u64()
                .filter(|n| (1..=1000).contains(n))
                .ok_or_else(|| ApiError::bad("rate_limit_per_hour must be 1..1000"))?;
            p.registration.rate_limit_per_hour = u32::try_from(n).unwrap_or(5);
        }
        if let Some(plan) = str_field(r, "default_plan_id", 64)? {
            let pl = plan.clone();
            let exists: bool = st
                .db
                .read(move |c| {
                    c.query_row("SELECT count(*) > 0 FROM plans WHERE id=?1", [&pl], |r| {
                        r.get(0)
                    })
                    .map_err(err)
                })
                .await?;
            if !exists {
                return Err(ApiError::bad("default_plan_id: unknown plan"));
            }
            p.registration.default_plan_id = plan;
        }
    }
    if let Some(s) = sec("smtp")? {
        if let Some(h) = str_field(s, "host", 253)? {
            if h.chars().any(|c| c.is_whitespace() || c == '/') {
                return Err(ApiError::bad("smtp.host must be a host name"));
            }
            p.smtp.host = h;
        }
        if let Some(v) = s.get("port") {
            let n = v
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| ApiError::bad("smtp.port must be 1..65535"))?;
            p.smtp.port = n;
        }
        if let Some(t) = str_field(s, "tls", 16)? {
            if !TLS_MODES.contains(&t.as_str()) {
                return Err(ApiError::bad("smtp.tls must be starttls, tls or none"));
            }
            p.smtp.tls = t;
        }
        if let Some(u) = str_field(s, "username", 320)? {
            p.smtp.username = u;
        }
        if let Some(pw) = str_field(s, "password", 1024)? {
            if pw.is_empty() {
                p.smtp.password_enc.clear();
            } else if pw != "***" {
                p.smtp.password_enc = sf_core::crypto::encrypt_secret(&st.secret_key, &pw)?;
            }
        }
        if let Some(f) = str_field(s, "from_email", 320)? {
            if !f.is_empty() && !f.contains('@') {
                return Err(ApiError::bad("smtp.from_email must be an email address"));
            }
            p.smtp.from_email = f;
        }
        if let Some(f) = str_field(s, "from_name", 120)? {
            if f.contains(['\r', '\n', '"']) {
                return Err(ApiError::bad("smtp.from_name contains invalid characters"));
            }
            p.smtp.from_name = f;
        }
    }
    if let Some(b) = sec("branding")? {
        if let Some(v) = str_field(b, "app_name", 60)? {
            p.branding.app_name = if v.is_empty() { "SiloForge".into() } else { v };
        }
        for (k, slot) in [
            ("logo_url", &mut p.branding.logo_url),
            ("terms_url", &mut p.branding.terms_url),
            ("privacy_url", &mut p.branding.privacy_url),
        ] {
            if let Some(v) = str_field(b, k, 2000)? {
                if !url_ok(&v) {
                    return Err(ApiError::bad(format!(
                        "branding.{k} must be an http(s) URL"
                    )));
                }
                *slot = v;
            }
        }
        if let Some(v) = str_field(b, "primary_color", 9)? {
            let ok = v.is_empty()
                || (v.starts_with('#')
                    && matches!(v.len(), 4 | 7)
                    && v.chars().skip(1).all(|c| c.is_ascii_hexdigit()));
            if !ok {
                return Err(ApiError::bad(
                    "branding.primary_color must be #rgb or #rrggbb",
                ));
            }
            p.branding.primary_color = v;
        }
        if let Some(v) = str_field(b, "support_email", 320)? {
            if !v.is_empty() && !v.contains('@') {
                return Err(ApiError::bad(
                    "branding.support_email must be an email address",
                ));
            }
            p.branding.support_email = v;
        }
    }
    if let Some(d) = sec("deployment")? {
        if let Some(v) = str_field(d, "public_url", 2000)? {
            if !url_ok(&v) {
                return Err(ApiError::bad(
                    "deployment.public_url must be an http(s) URL",
                ));
            }
            p.deployment.public_url = v;
        }
        if let Some(b) = bool_field(d, "trust_proxy")? {
            p.deployment.trust_proxy = b;
        }
    }
    if let Some(b) = sec("billing")?
        && let Some(v) = str_field(b, "provider", 16)?
    {
        if !BILLING.contains(&v.as_str()) {
            return Err(ApiError::bad(
                "billing.provider must be none, manual or stripe",
            ));
        }
        p.billing.provider = v;
    }
    let a = serde_json::to_value(&before).unwrap_or_default();
    let b = serde_json::to_value(&p).unwrap_or_default();
    let mut changed = Vec::new();
    if let (Some(a), Some(b)) = (a.as_object(), b.as_object()) {
        for (sec, bv) in b {
            if let (Some(am), Some(bm)) = (a.get(sec).and_then(Value::as_object), bv.as_object()) {
                for (k, v) in bm {
                    if am.get(k) != Some(v) {
                        changed.push(format!("{sec}.{k}"));
                    }
                }
            }
        }
    }
    save(st, &p).await?;
    Ok((p, changed))
}

/// Email domain allowed by the registration allow/deny lists.
pub fn domain_allowed(p: &Platform, email: &str) -> bool {
    let dom = email.rsplit('@').next().unwrap_or("").to_ascii_lowercase();
    let matches = |list: &[String]| {
        list.iter()
            .any(|d| dom == *d || dom.ends_with(&format!(".{d}")))
    };
    if matches(&p.registration.denied_domains) {
        return false;
    }
    p.registration.allowed_domains.is_empty() || matches(&p.registration.allowed_domains)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_domains() {
        let mut p = Platform::default();
        assert_eq!(p.mode(false), "closed");
        assert_eq!(p.mode(true), "open");
        p.registration.mode = Some("invite_only".into());
        assert_eq!(p.mode(true), "invite_only");
        assert!(domain_allowed(&p, "a@example.com"));
        p.registration.denied_domains = vec!["spam.test".into()];
        assert!(!domain_allowed(&p, "a@x.spam.test"));
        p.registration.allowed_domains = vec!["example.com".into()];
        assert!(domain_allowed(&p, "a@example.com"));
        assert!(!domain_allowed(&p, "a@other.org"));
        let j = p.to_json(false);
        assert_eq!(j["smtp"]["password"], "");
    }
}
