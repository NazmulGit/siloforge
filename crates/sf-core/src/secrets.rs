//! Per-site credentials, stored encrypted in `sites.secrets_enc` (never in `settings`, never returned by the API).
//! v1 kept `settings.wp_user` / `settings.wp_app_password` / `settings.gsc.credentials` in plaintext; the
//! migrator and `PATCH /sites/:id` move them here.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::crypto::{decrypt_secret, encrypt_secret};

#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SiteSecrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wp_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wp_app_password: Option<String>,
    /// GSC service-account JSON (as a string).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gsc_credentials: Option<String>,
}

/// L-14: never print credential values (only whether they are set, plus the WP user name).
impl std::fmt::Debug for SiteSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |o: &Option<String>| o.as_ref().map(|_| "<redacted>");
        f.debug_struct("SiteSecrets")
            .field("wp_user", &self.wp_user)
            .field("wp_app_password", &set(&self.wp_app_password))
            .field("gsc_credentials", &set(&self.gsc_credentials))
            .finish()
    }
}

impl SiteSecrets {
    pub fn load(key: &[u8; 32], blob: &str) -> Result<Self> {
        if blob.is_empty() {
            return Ok(Self::default());
        }
        let json = decrypt_secret(key, blob)?;
        Ok(serde_json::from_str(&json).unwrap_or_default())
    }

    pub fn seal(&self, key: &[u8; 32]) -> Result<String> {
        if *self == Self::default() {
            return Ok(String::new());
        }
        let json =
            serde_json::to_string(self).map_err(|e| crate::Error::internal(e.to_string()))?;
        encrypt_secret(key, &json)
    }

    pub fn has_wp_credentials(&self) -> bool {
        matches!((&self.wp_user, &self.wp_app_password), (Some(u), Some(p)) if !u.is_empty() && !p.is_empty())
    }

    /// Removes credential keys from a v1-style settings object, moving them into `self`.
    /// Returns true when anything was moved.
    pub fn absorb_from_settings(&mut self, settings: &mut serde_json::Value) -> bool {
        let Some(obj) = settings.as_object_mut() else {
            return false;
        };
        let mut moved = false;
        if let Some(v) = obj.remove("wp_user") {
            if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
                self.wp_user = Some(s.to_string());
            }
            moved = true;
        }
        if let Some(v) = obj.remove("wp_app_password") {
            if let Some(s) = v.as_str().filter(|s| !s.is_empty() && *s != "***") {
                self.wp_app_password = Some(s.to_string());
            }
            moved = true;
        }
        if let Some(gsc) = obj.get_mut("gsc").and_then(|g| g.as_object_mut())
            && let Some(c) = gsc.remove("credentials")
        {
            match c {
                serde_json::Value::String(s) if !s.is_empty() && s != "***" => {
                    self.gsc_credentials = Some(s)
                }
                serde_json::Value::Object(_) => self.gsc_credentials = Some(c.to_string()),
                _ => {}
            }
            moved = true;
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absorb_and_seal() {
        let key = crate::crypto::derive_secret_key("k").unwrap();
        let mut settings = serde_json::json!({"wp_user":"bob","wp_app_password":"abcd efgh","insert_modules":["native"],
            "gsc":{"connected":true,"credentials":{"private_key":"x"}}});
        let mut s = SiteSecrets::default();
        assert!(s.absorb_from_settings(&mut settings));
        assert!(
            settings.get("wp_app_password").is_none()
                && settings["gsc"].get("credentials").is_none()
        );
        assert!(s.has_wp_credentials());
        let blob = s.seal(&key).unwrap();
        assert!(!blob.contains("abcd"));
        assert_eq!(SiteSecrets::load(&key, &blob).unwrap(), s);
        assert_eq!(SiteSecrets::default().seal(&key).unwrap(), "");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("abcd efgh") && !dbg.contains("\"x\"") && dbg.contains("bob"));
    }
}
