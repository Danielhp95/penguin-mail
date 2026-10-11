//! The change feed: the Email and Mailbox states, joined in one sync state.
//! Each created or updated message comes restated as it stands, with every
//! mailbox and keyword it has as a gain and every known one it lacks as a
//! loss. Any mailbox change relists the mailboxes, which brings the
//! sidebar's counts. A state the server no longer keeps answers
//! `StateLost`.

use std::collections::BTreeSet;

use mailrs_domain::Membership;
use mailrs_jmap::Email;
use serde::{Deserialize, Serialize};

use super::{Jmap, JmapApi, KEYWORDS, backend};
use crate::BackendError;
use crate::services::{Changes, RemoteChange, SyncState};

/// The most pages one look reads: five thousand changes, after which the
/// next look carries on from the state reached.
const PAGES_PER_LOOK: usize = 10;

const CHANGES_PER_PAGE: usize = 500;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct FeedState {
    pub email: String,
    pub mailbox: String,
}

impl FeedState {
    fn read(state: &SyncState) -> Result<FeedState, BackendError> {
        serde_json::from_str(state.as_str()).map_err(|_| BackendError::StateLost)
    }

    fn written(&self) -> SyncState {
        SyncState::new(serde_json::to_string(self).unwrap_or_default())
    }
}

impl<J: JmapApi> Jmap<J> {
    pub(super) async fn feed(&self, since: Option<&SyncState>) -> Result<Changes, BackendError> {
        let Some(since) = since else {
            let email = self.api().email_state().await.map_err(backend)?;
            self.list_mailboxes().await?;
            let mailbox = self.known().mailbox_state.clone();
            return Ok(Changes { changes: Vec::new(), state: FeedState { email, mailbox }.written() });
        };
        let mut state = FeedState::read(since)?;
        self.listed().await?;
        let mut changes = Vec::new();
        let mut relist = false;
        for _ in 0..PAGES_PER_LOOK {
            let page = self.api().mail_changes(&state.email, &state.mailbox, CHANGES_PER_PAGE).await.map_err(backend)?;
            // Any mailbox change relists: a new or renamed mailbox, and the
            // counts the sidebar shows, all come from one Mailbox/get.
            relist |= !page.mailbox.created.is_empty() || !page.mailbox.updated.is_empty() || !page.mailbox.destroyed.is_empty();
            for email in &page.changed {
                changes.extend(self.restated(email));
            }
            changes.extend(page.email.destroyed.iter().map(|id| RemoteChange::Deleted { id: id.clone() }));
            state.email = page.email.new_state;
            state.mailbox = page.mailbox.new_state;
            if !page.email.has_more_changes {
                break;
            }
        }
        if relist {
            self.list_mailboxes().await?;
        }
        Ok(Changes { changes, state: state.written() })
    }

    /// `email` as it stands: a gain of each mailbox and keyword it has, a
    /// loss of each known mailbox and keyword it lacks.
    fn restated(&self, email: &Email) -> Vec<RemoteChange> {
        let has_mailbox: BTreeSet<&str> = email.mailbox_ids.iter().filter(|(_, on)| **on).map(|(id, _)| id.as_str()).collect();
        let has_keyword: BTreeSet<&str> = email.keywords.iter().filter(|(_, on)| **on).map(|(k, _)| k.as_str()).collect();
        let mut gained: Vec<Membership> = has_mailbox.iter().map(|id| Membership::Mailbox((*id).to_string())).collect();
        gained.extend(has_keyword.iter().map(|k| Membership::Keyword((*k).to_string())));
        let mut lost: Vec<Membership> = self
            .known()
            .mailboxes
            .keys()
            .filter(|id| !has_mailbox.contains(id.as_str()))
            .map(|id| Membership::Mailbox(id.clone()))
            .collect();
        lost.extend(KEYWORDS.iter().filter(|k| !has_keyword.contains(**k)).map(|k| Membership::Keyword((*k).to_string())));
        let mut out = vec![RemoteChange::Gained { id: email.id.clone(), thread_id: email.thread_id.clone(), memberships: gained }];
        if !lost.is_empty() {
            out.push(RemoteChange::Lost { id: email.id.clone(), memberships: lost });
        }
        out
    }
}
