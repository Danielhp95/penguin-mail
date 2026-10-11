//! The account's mailboxes: roles from IANA's mailbox-attribute registry,
//! paths built from each parent, the counts the sidebar shows, and the
//! person's own mailboxes made, renamed and deleted.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use mailrs_domain::{MailboxKind, RemoteMailbox, Role};
use mailrs_jmap::{Mailbox, MailboxWrite};

use super::{Jmap, JmapApi, backend};
use crate::BackendError;

/// How deep a path is followed before the walk stops, against a server
/// whose parents form a loop.
const MOST_DEPTH: usize = 32;

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
        for mailbox in &listed.list {
            let role = mailbox.role.as_deref().and_then(|r| Role::from_str(r).ok());
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
        let id = self.api().set_mailbox(&MailboxWrite::Create { name, parent_id }).await.map_err(backend)?;
        let listed = self.list_mailboxes().await?;
        self.listed_as(&id, listed)
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
