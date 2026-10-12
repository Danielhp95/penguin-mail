//! The account's mailboxes: roles from IANA's mailbox-attribute registry,
//! paths built from each parent, the counts the sidebar shows, and the
//! person's own mailboxes made, renamed and deleted.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use mailrs_domain::{MailboxKind, RemoteMailbox, Role};
use mailrs_jmap::{JmapError, Mailbox, MailboxWrite};

use super::{Jmap, JmapApi, backend};
use crate::BackendError;

/// How deep a path is followed before the walk stops, against a server
/// whose parents form a loop.
const MOST_DEPTH: usize = 32;

/// The name of the Archive the adapter makes, and of a top-level mailbox
/// that counts as Archive when no mailbox has the role.
const ARCHIVE: &str = "Archive";

/// A top-level mailbox named Archive with no role, which stands in for
/// Archive on a server that marks none, as the IMAP adapter reads it.
fn plain_archive(list: &[Mailbox]) -> Option<&str> {
    if list.iter().any(|m| m.role.as_deref() == Some("archive")) {
        return None;
    }
    list.iter()
        .find(|m| m.parent_id.is_none() && m.role.is_none() && m.name.eq_ignore_ascii_case(ARCHIVE))
        .map(|m| m.id.as_str())
}

fn path_of(id: &str, by_id: &HashMap<String, &Mailbox>) -> String {
    let mut names = Vec::new();
    let mut at = by_id.get(id).copied();
    while let Some(mailbox) = at {
        names.push(mailbox.name.clone());
        if names.len() >= MOST_DEPTH {
            break;
        }
        at = mailbox.parent_id.as_deref().and_then(|p| by_id.get(p).copied());
    }
    names.reverse();
    names.join("/")
}

impl<J: JmapApi> Jmap<J> {
    /// Every mailbox with its role and path. Refreshes what the adapter
    /// knows, the counts included.
    pub(super) async fn list_mailboxes(&self) -> Result<Vec<RemoteMailbox>, BackendError> {
        let listed = self.api().mailboxes().await.map_err(backend)?;
        let labels = self.settings().labels;
        let by_id: HashMap<String, &Mailbox> = listed.list.iter().map(|m| (m.id.clone(), m)).collect();
        let mut roles = BTreeMap::new();
        let mut paths = HashMap::new();
        let mut out = Vec::with_capacity(listed.list.len());
        let plain_archive = plain_archive(&listed.list);
        for mailbox in &listed.list {
            let role = match plain_archive == Some(mailbox.id.as_str()) {
                true => Some(Role::Archive),
                false => mailbox.role.as_deref().and_then(|r| Role::from_str(r).ok()),
            };
            if let Some(role) = role {
                roles.entry(role).or_insert_with(|| mailbox.id.clone());
            }
            let name = path_of(&mailbox.id, &by_id);
            paths.insert(mailbox.id.clone(), name.clone());
            let kind = match (role, labels) {
                (Some(_), _) => MailboxKind::System,
                (None, true) => MailboxKind::Label,
                (None, false) => MailboxKind::Folder,
            };
            out.push(RemoteMailbox { id: mailbox.id.clone(), name, kind, role, color: None, hidden: false });
        }
        let mut known = self.known();
        known.roles = roles;
        known.paths = paths;
        known.mailboxes = listed.list.into_iter().map(|m| (m.id.clone(), m)).collect();
        known.mailbox_state = listed.state;
        known.listed = true;
        Ok(out)
    }

    /// Lists the mailboxes once, the first time anything needs them.
    pub(super) async fn listed(&self) -> Result<(), BackendError> {
        if !self.known().listed {
            self.list_mailboxes().await?;
        }
        Ok(())
    }

    /// The parent's id and the last name of `path`, `Parent/Child`.
    fn place_of(&self, path: &str) -> Result<(Option<String>, String), BackendError> {
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent, name)) => (Some(parent), name),
            None => (None, path),
        };
        let parent_id = match parent {
            None => None,
            Some(parent) => {
                let known = self.known();
                let found = known.paths.iter().find(|(_, p)| p.as_str() == parent).map(|(id, _)| id.clone());
                Some(found.ok_or(BackendError::NotFound)?)
            }
        };
        Ok((parent_id, name.to_string()))
    }

    fn listed_as(&self, id: &str, listed: Vec<RemoteMailbox>) -> Result<RemoteMailbox, BackendError> {
        listed.into_iter().find(|m| m.id == id).ok_or(BackendError::NotFound)
    }

    pub(super) async fn make_mailbox(&self, path: &str) -> Result<RemoteMailbox, BackendError> {
        self.listed().await?;
        let (parent_id, name) = self.place_of(path)?;
        let id = self.api().set_mailbox(&MailboxWrite::Create { name, parent_id, role: None }).await.map_err(backend)?;
        let listed = self.list_mailboxes().await?;
        self.listed_as(&id, listed)
    }

    /// Archive's id, made first when the account has none, as a new
    /// Stalwart account has not. A server that refuses the role gets a
    /// plain top-level "Archive", which then counts as Archive. When the
    /// make fails, maybe because another client made one meanwhile, a
    /// fresh listing says whether there is one now.
    pub(super) async fn ensure_archive(&self) -> Result<Option<String>, BackendError> {
        if let Some(id) = self.known().roles.get(&Role::Archive) {
            return Ok(Some(id.clone()));
        }
        let make = |role: Option<&str>| MailboxWrite::Create { name: ARCHIVE.into(), parent_id: None, role: role.map(str::to_string) };
        let made = match self.api().set_mailbox(&make(Some("archive"))).await {
            Err(JmapError::Method { kind, .. }) if kind == "invalidProperties" => self.api().set_mailbox(&make(None)).await,
            answer => answer,
        };
        if let Err(err) = made {
            tracing::warn!(error = %backend(err), "the account's Archive mailbox could not be made");
        }
        self.list_mailboxes().await?;
        Ok(self.known().roles.get(&Role::Archive).cloned())
    }

    pub(super) async fn move_mailbox(&self, id: &str, path: &str) -> Result<RemoteMailbox, BackendError> {
        self.listed().await?;
        let (parent_id, name) = self.place_of(path)?;
        self.api().set_mailbox(&MailboxWrite::Update { id: id.to_string(), name, parent_id }).await.map_err(backend)?;
        let listed = self.list_mailboxes().await?;
        self.listed_as(id, listed)
    }

    pub(super) async fn remove_mailbox(&self, id: &str) -> Result<(), BackendError> {
        self.api().set_mailbox(&MailboxWrite::Destroy { id: id.to_string() }).await.map_err(backend)?;
        self.list_mailboxes().await.map(drop)
    }
}
