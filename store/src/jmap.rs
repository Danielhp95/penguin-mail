//! Where a JMAP account's mail lives: its session URL and how it signs in.
//! The password or token is in the keyring and never here.

use mailrs_domain::AccountId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{Result, StoreError};

/// How a JMAP account signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JmapAuth {
    /// The account password over HTTP Basic.
    Basic,
    /// A token over HTTP Bearer.
    Bearer,
    /// A token from an OAuth sign-in, refreshed at the token endpoint.
    OAuth,
}

impl JmapAuth {
    /// The stored form, in `jmap_accounts.auth`.
    pub fn as_str(self) -> &'static str {
        match self {
            JmapAuth::Basic => "basic",
            JmapAuth::Bearer => "bearer",
            JmapAuth::OAuth => "oauth",
        }
    }

    pub fn parse(stored: &str) -> Option<JmapAuth> {
        [JmapAuth::Basic, JmapAuth::Bearer, JmapAuth::OAuth].into_iter().find(|a| a.as_str() == stored)
    }
}

/// A JMAP account's row: what it needs to reach its session again. Its
/// `Debug` leaves the user name out, which is often an address.
#[derive(Clone, PartialEq, Eq)]
pub struct JmapSaved {
    pub session_url: String,
    pub auth: JmapAuth,
    /// The user name Basic sends, when it is not the address.
    pub user_name: Option<String>,
    pub labels: bool,
    /// The capabilities the session offered this sign-in, by URN.
    pub capabilities: Vec<String>,
    /// The client and token endpoint of an OAuth sign-in. The tokens and
    /// any client secret are in the keyring.
    pub oauth: Option<SavedOAuth>,
    /// Who the session belonged to at the last sign-in. A row kept before
    /// the app recorded it has none.
    pub owner: Option<SessionOwner>,
}

/// Who a JMAP session belongs to: the session's `username` and the id of
/// its primary mail account. A sign-in that replaces an account's
/// credentials must reach the same owner. Its `Debug` leaves the user name
/// out, which is often an address.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionOwner {
    pub username: String,
    pub mail_account: String,
}

impl std::fmt::Debug for SessionOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOwner").field("username", &"<hidden>").field("mail_account", &self.mail_account).finish()
    }
}

/// The `capabilities` column. A row with an owner keeps it beside the
/// capabilities in one object; an older row holds the bare list.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum StoredCapabilities {
    List(Vec<String>),
    #[serde(rename_all = "camelCase")]
    WithOwner { offered: Vec<String>, username: String, mail_account: String },
}

/// What an OAuth sign-in needs besides its tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedOAuth {
    pub client_id: String,
    pub token_endpoint: String,
}

impl std::fmt::Debug for JmapSaved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JmapSaved")
            .field("session_url", &self.session_url)
            .field("auth", &self.auth)
            .field("user_name", &self.user_name.as_ref().map(|_| "<hidden>"))
            .field("labels", &self.labels)
            .field("capabilities", &self.capabilities)
            .field("oauth", &self.oauth.as_ref().map(|_| "<hidden>"))
            .field("owner", &self.owner)
            .finish()
    }
}

impl JmapSaved {
    pub fn offers(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

pub fn save(conn: &Connection, account_id: AccountId, saved: &JmapSaved) -> Result<()> {
    let stored = match &saved.owner {
        Some(owner) => StoredCapabilities::WithOwner {
            offered: saved.capabilities.clone(),
            username: owner.username.clone(),
            mail_account: owner.mail_account.clone(),
        },
        None => StoredCapabilities::List(saved.capabilities.clone()),
    };
    let capabilities = serde_json::to_string(&stored).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO jmap_accounts (account_id, session_url, auth, user_name, labels, capabilities, oauth_client_id, oauth_token_url) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (account_id) DO UPDATE SET session_url = excluded.session_url, auth = excluded.auth, \
         user_name = excluded.user_name, labels = excluded.labels, capabilities = excluded.capabilities, \
         oauth_client_id = excluded.oauth_client_id, oauth_token_url = excluded.oauth_token_url",
        params![
            account_id,
            saved.session_url,
            saved.auth.as_str(),
            saved.user_name,
            saved.labels,
            capabilities,
            saved.oauth.as_ref().map(|o| &o.client_id),
            saved.oauth.as_ref().map(|o| &o.token_endpoint),
        ],
    )?;
    Ok(())
}

pub fn load(conn: &Connection, account_id: AccountId) -> Result<Option<JmapSaved>> {
    type Row = (String, String, Option<String>, bool, String, Option<String>, Option<String>);
    let row: Option<Row> = conn
        .query_row(
            "SELECT session_url, auth, user_name, labels, capabilities, oauth_client_id, oauth_token_url \
             FROM jmap_accounts WHERE account_id = ?1",
            params![account_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .optional()?;
    row.map(|(session_url, auth, user_name, labels, capabilities, client_id, token_endpoint)| {
        let auth = JmapAuth::parse(&auth).ok_or(StoreError::Corrupt { column: "jmap_accounts.auth", value: auth })?;
        let stored = serde_json::from_str(&capabilities)
            .map_err(|_| StoreError::Corrupt { column: "jmap_accounts.capabilities", value: capabilities })?;
        let (capabilities, owner) = match stored {
            StoredCapabilities::List(offered) => (offered, None),
            StoredCapabilities::WithOwner { offered, username, mail_account } => (offered, Some(SessionOwner { username, mail_account })),
        };
        let oauth = client_id.zip(token_endpoint).map(|(client_id, token_endpoint)| SavedOAuth { client_id, token_endpoint });
        Ok(JmapSaved { session_url, auth, user_name, labels, capabilities, oauth, owner })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{accounts, open_in_memory};

    #[test]
    fn a_saved_session_comes_back() {
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_jmap_account(&conn, "me@fastmail.com", "Fastmail", 0).unwrap().unwrap();
        assert_eq!(load(&conn, id).unwrap(), None);
        let saved = JmapSaved {
            session_url: "https://api.fastmail.com/jmap/session".into(),
            auth: JmapAuth::Bearer,
            user_name: None,
            labels: true,
            capabilities: vec!["urn:ietf:params:jmap:mail".into(), "urn:ietf:params:jmap:submission".into()],
            oauth: None,
            owner: None,
        };
        save(&conn, id, &saved).unwrap();
        assert_eq!(load(&conn, id).unwrap(), Some(saved.clone()));
        let basic = JmapSaved { auth: JmapAuth::Basic, user_name: Some("me".into()), ..saved };
        save(&conn, id, &basic).unwrap();
        assert_eq!(load(&conn, id).unwrap(), Some(basic));
    }

    #[test]
    fn an_oauth_sign_in_keeps_its_client() {
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_jmap_account(&conn, "me@fastmail.com", "Fastmail", 0).unwrap().unwrap();
        let saved = JmapSaved {
            session_url: "https://api.fastmail.com/jmap/session".into(),
            auth: JmapAuth::OAuth,
            user_name: None,
            labels: true,
            capabilities: Vec::new(),
            oauth: Some(SavedOAuth { client_id: "c1".into(), token_endpoint: "https://api.fastmail.com/oauth/refresh".into() }),
            owner: None,
        };
        save(&conn, id, &saved).unwrap();
        assert_eq!(load(&conn, id).unwrap(), Some(saved));
    }

    #[test]
    fn the_session_s_owner_comes_back_beside_the_capabilities() {
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_jmap_account(&conn, "me@fastmail.com", "Fastmail", 0).unwrap().unwrap();
        let saved = JmapSaved {
            session_url: "https://api.fastmail.com/jmap/session".into(),
            auth: JmapAuth::OAuth,
            user_name: None,
            labels: true,
            capabilities: vec!["urn:ietf:params:jmap:mail".into()],
            oauth: None,
            owner: Some(SessionOwner { username: "me@fastmail.com".into(), mail_account: "u1".into() }),
        };
        save(&conn, id, &saved).unwrap();
        assert_eq!(load(&conn, id).unwrap(), Some(saved));
    }

    #[test]
    fn a_row_kept_before_the_owner_was_recorded_has_none() {
        let conn = open_in_memory().unwrap();
        let id = accounts::insert_jmap_account(&conn, "me@fastmail.com", "Fastmail", 0).unwrap().unwrap();
        conn.execute(
            "INSERT INTO jmap_accounts (account_id, session_url, auth, labels, capabilities) VALUES (?1, 'https://a.test/jmap', 'bearer', 1, '[\"urn:ietf:params:jmap:mail\"]')",
            params![id],
        )
        .unwrap();
        let saved = load(&conn, id).unwrap().unwrap();
        assert_eq!((saved.capabilities, saved.owner), (vec!["urn:ietf:params:jmap:mail".to_string()], None));
    }

    #[test]
    fn debug_output_leaves_the_owner_s_user_name_out() {
        let owner = SessionOwner { username: "dana@example.org".into(), mail_account: "u1".into() };
        assert!(!format!("{owner:?}").contains("dana"));
    }

    #[test]
    fn debug_output_leaves_the_user_name_out() {
        let saved = JmapSaved {
            session_url: "https://mail.example.org/jmap/session".into(),
            auth: JmapAuth::Basic,
            user_name: Some("dana@example.org".into()),
            labels: false,
            capabilities: vec![],
            oauth: None,
            owner: None,
        };
        let shown = format!("{saved:?}");
        assert!(!shown.contains("dana"), "{shown}");
        assert!(shown.contains("https://mail.example.org/jmap/session"), "{shown}");
    }

    #[test]
    fn an_address_another_provider_holds_is_not_taken() {
        let conn = open_in_memory().unwrap();
        accounts::insert_imap_account(&conn, "me@fastmail.com", "Fastmail", 0).unwrap();
        assert_eq!(accounts::insert_jmap_account(&conn, "me@fastmail.com", "Fastmail", 0).unwrap(), None);
    }
}
