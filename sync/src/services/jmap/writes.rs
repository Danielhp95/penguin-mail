//! Moves, marks and Delete Forever as `Email/set` patches. A patch names
//! only what it changes, so another client's change to the same message
//! stays. A message never sits in no mailbox: taking it out of its last
//! one files it in Archive. A write the server takes for some messages and
//! refuses for one says how many from the front went through.

use std::collections::BTreeSet;

use mailrs_domain::Role;
use mailrs_domain::translate::gettext;
use mailrs_jmap::{SetResponse, pointer};
use serde_json::{Map, Value, json};

use super::{Jmap, JmapApi, backend};
use crate::services::{Relocated, Unapplied};
use crate::{BackendError, MailOp};

/// The most messages one `Email/set` names, under every server's
/// `maxObjectsInSet` seen.
const PER_SET: usize = 500;

/// One write's patch, the same for every message it names.
struct Patch {
    fields: Map<String, Value>,
    /// Mailboxes the write takes messages out of while it puts them in
    /// none: each message needs a look before it goes.
    leaves: BTreeSet<String>,
}

fn unapplied(taken: usize, error: BackendError) -> Unapplied {
    Unapplied { taken, error, relocated: Vec::new() }
}

/// Puts `id` in or takes it out of the patch, inside a whole
/// `mailboxIds` when a move already set one: a patch may not name a path
/// and a path under it.
fn put_mailbox(fields: &mut Map<String, Value>, id: &str, on: bool) {
    if let Some(Value::Object(whole)) = fields.get_mut("mailboxIds") {
        match on {
            true => whole.insert(id.to_string(), json!(true)),
            false => whole.remove(id),
        };
        return;
    }
    fields.insert(format!("mailboxIds/{}", pointer(id)), if on { json!(true) } else { Value::Null });
}

fn set_whole(fields: &mut Map<String, Value>, id: &str) {
    fields.retain(|key, _| !key.starts_with("mailboxIds"));
    let mut whole = Map::new();
    whole.insert(id.to_string(), json!(true));
    fields.insert("mailboxIds".into(), Value::Object(whole));
}

/// The first message of `chunk`, in order, the server refused, and why.
/// A message the server no longer has counts as done.
fn first_refusal(chunk: &[String], answer: &SetResponse) -> Option<(usize, BackendError)> {
    chunk.iter().enumerate().find_map(|(at, id)| {
        let refused = answer.refusal(id).filter(|e| e.kind != "notFound")?;
        Some((at, backend(refused.error())))
    })
}

impl<J: JmapApi> Jmap<J> {
    fn patch_of(&self, ops: &[MailOp]) -> Result<Patch, BackendError> {
        let known = self.known();
        let role_id = |role: &Role| {
            known.roles.get(role).cloned().ok_or_else(|| BackendError::Refused(gettext("The server has no mailbox for this.")))
        };
        let mut fields = Map::new();
        let mut leaves = BTreeSet::new();
        let mut puts_somewhere = false;
        for op in ops {
            match op {
                MailOp::AddToMailbox(id) => {
                    put_mailbox(&mut fields, id, true);
                    puts_somewhere = true;
                }
                MailOp::RemoveFromMailbox(id) => {
                    put_mailbox(&mut fields, id, false);
                    leaves.insert(id.clone());
                }
                MailOp::MoveToRole(role) => {
                    set_whole(&mut fields, &role_id(role)?);
                    puts_somewhere = true;
                }
                MailOp::MoveToMailbox(id) => {
                    set_whole(&mut fields, id);
                    puts_somewhere = true;
                }
                MailOp::SetKeyword { keyword, on } => {
                    fields.insert(format!("keywords/{}", pointer(keyword)), if *on { json!(true) } else { Value::Null });
                }
                MailOp::SetCategory { .. } => {
                    return Err(BackendError::Refused(gettext("This server does not sort mail into categories.")));
                }
                MailOp::Destroy => {}
            }
        }
        if puts_somewhere {
            leaves.clear();
        }
        Ok(Patch { fields, leaves })
    }

    /// Each message's patch: the write's own and, for a message it would
    /// take out of every mailbox, Archive in their place.
    async fn updates_for(&self, chunk: &[String], patch: &Patch) -> Result<Map<String, Value>, BackendError> {
        let mut update = Map::new();
        if patch.leaves.is_empty() {
            for id in chunk {
                update.insert(id.clone(), Value::Object(patch.fields.clone()));
            }
            return Ok(update);
        }
        let got = self.api().emails(chunk, &["id", "mailboxIds"]).await.map_err(backend)?;
        let archive = self.known().roles.get(&Role::Archive).cloned();
        for email in got.list {
            let stays = email.mailbox_ids.iter().any(|(id, on)| *on && !patch.leaves.contains(id));
            let mut fields = patch.fields.clone();
            if !stays {
                let archive = archive.clone().ok_or_else(|| {
                    BackendError::Refused(gettext("A message must stay in at least one mailbox, and this server has no Archive."))
                })?;
                put_mailbox(&mut fields, &archive, true);
            }
            update.insert(email.id, Value::Object(fields));
        }
        Ok(update)
    }

    pub(super) async fn write(&self, messages: &[String], ops: &[MailOp]) -> Result<Vec<Relocated>, Unapplied> {
        self.listed().await.map_err(|e| unapplied(0, e))?;
        let destroy = ops.iter().any(|op| matches!(op, MailOp::Destroy));
        let patch = match destroy {
            true => None,
            false => Some(self.patch_of(ops).map_err(|e| unapplied(0, e))?),
        };
        let mut taken = 0;
        for chunk in messages.chunks(PER_SET) {
            let answer = match &patch {
                None => self.api().set_emails(&Map::new(), chunk).await,
                Some(patch) => {
                    let update = self.updates_for(chunk, patch).await.map_err(|e| unapplied(taken, e))?;
                    self.api().set_emails(&update, &[]).await
                }
            }
            .map_err(|e| unapplied(taken, backend(e)))?;
            if let Some((at, error)) = first_refusal(chunk, &answer) {
                return Err(unapplied(taken + at, error));
            }
            taken += chunk.len();
        }
        Ok(Vec::new())
    }
}
