//! Which new mail a notification names, and which still deserves one
//! once the local rules ran.

/// Whether new mail still deserves its notification once the rules ran:
/// still in the Inbox and still unread.
pub(crate) fn still_news(in_inbox: bool, unread: bool) -> bool {
    in_inbox && unread
}

/// The new messages a notification names, oldest first, read in one
/// query. Each used to load its whole thread to pick itself out of it.
pub(crate) fn announced(
    c: &rusqlite::Connection,
    account_id: mailrs_domain::AccountId,
    message_ids: &[String],
) -> mailrs_store::Result<Vec<mailrs_domain::MessageMeta>> {
    mailrs_store::messages::by_ids(c, account_id, message_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_announced_mail_is_the_new_messages_and_not_their_threads() {
        use crate::assistant::run::fake::meta;
        use mailrs_store::messages::{Change, apply};

        let dir = tempfile::tempdir().unwrap();
        let db = mailrs_store::Db::open(&dir.path().join("mail.db")).unwrap();
        let found = db
            .write(|c| {
                let account_id = mailrs_store::accounts::insert_account(c, "me@example.com", 0)?;
                let changes: Vec<Change> = [("a", 1), ("b", 2), ("c", 3)]
                    .into_iter()
                    .map(|(id, at)| Change::Upsert {
                        meta: Box::new(meta(id, "t", "ann@example.com", "Kites", at)),
                        generation: 1,
                    })
                    .collect();
                apply(c, account_id, &changes)?;
                announced(c, account_id, &["c".to_string(), "a".to_string(), "gone".to_string()])
            })
            .await
            .unwrap();
        let ids: Vec<&str> = found.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"], "oldest first, and nothing the store lost");
    }

    #[test]
    fn mail_a_rule_moved_or_read_is_not_announced() {
        assert!(still_news(true, true));
        assert!(!still_news(false, true), "a rule moved it out of the Inbox");
        assert!(!still_news(true, false), "a rule marked it read");
    }
}
