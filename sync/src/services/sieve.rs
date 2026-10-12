//! Rules and the automatic reply on a server that runs Sieve, through
//! ManageSieve. Both live in the one script Penguin Mail keeps there,
//! `penguin-mail`, which every change reads, edits and writes whole. A
//! script the person already runs is included where the server can
//! include; otherwise the first write answers `WouldReplace` until the
//! person says yes. Folder names go to the server in UTF-8, decoded from
//! the IMAP adapter's modified UTF-7 ids.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mailrs_domain::translate::{fill, gettext};
use mailrs_domain::{Filter, MailSet, Vacation};
use mailrs_sieve::SCRIPT_NAME;
use mailrs_sieve::client::{ManageSieveApi, SieveError};
use mailrs_sieve::script::{self, Extensions, Script, Unsayable, WriteError};

use super::{AnyMail, AutoReplyService, MailBackend, RulesService};
use crate::BackendError;

pub struct SieveRules<M> {
    api: Arc<M>,
    /// The account's mail, which names the folder a role files into.
    mail: AnyMail,
    address: String,
    provider: String,
    state: Arc<Mutex<State>>,
    /// The script a JMAP server keeps its own automatic reply in. Another
    /// client's script is included, but never this one: the reply would
    /// keep answering while the server reports it off.
    server_reply: Option<&'static str>,
    /// Held from a write's read of the script through its put, so a rule
    /// sent by the timer and the automatic reply saved at the same moment
    /// do not both start from the same script and lose one change.
    writing: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct State {
    take_over: bool,
    extensions: Option<Extensions>,
}

impl<M> Clone for SieveRules<M> {
    fn clone(&self) -> Self {
        SieveRules {
            api: Arc::clone(&self.api),
            mail: self.mail.clone(),
            address: self.address.clone(),
            provider: self.provider.clone(),
            state: Arc::clone(&self.state),
            server_reply: self.server_reply,
            writing: Arc::clone(&self.writing),
        }
    }
}

/// Which script the server runs now.
enum Running {
    Ours,
    Theirs(String),
    Nothing,
}

impl<M: ManageSieveApi> SieveRules<M> {
    pub fn new(api: Arc<M>, mail: AnyMail, address: String, provider: String) -> Self {
        SieveRules {
            api,
            mail,
            address,
            provider,
            state: Arc::default(),
            server_reply: None,
            writing: Arc::default(),
        }
    }

    /// These rules on a server that keeps its own automatic reply in the
    /// script named `script`.
    pub fn beside_server_reply(self, script: Option<&'static str>) -> Self {
        SieveRules { server_reply: script, ..self }
    }

    fn is_server_reply(&self, name: &str) -> bool {
        self.server_reply == Some(name)
    }

    /// Takes out an include of the server's own reply script, which an
    /// older build wrote.
    fn drop_server_reply(&self, script: &mut Script) {
        if script.include.as_deref().is_some_and(|name| self.is_server_reply(name)) {
            script.include = None;
        }
    }

    /// The rules, and the text on the server nobody here wrote, in one
    /// read of the script.
    pub async fn listing(&self) -> Result<(Vec<Filter>, Vec<String>), BackendError> {
        let (script, _, _) = self.load().await?;
        Ok((script.rules, script.foreign))
    }

    /// Brings the script in line with the account's folders after one was
    /// renamed, moved or deleted. `moved` pairs a folder's old id with its
    /// new one, as an IMAP rename changes it; a JMAP id stays, and only the
    /// path the script names changes. A rule whose folder is gone is turned
    /// off rather than left filing into nothing, and the Rules page says
    /// so. The script is written only when its text changes, and is never
    /// made the running one here.
    pub async fn follow_folders(&self, moved: &[(String, String)]) -> Result<(), BackendError> {
        let _writing = self.writing.lock().await;
        let (mut script, ext, _, held) = self.load_text().await?;
        let Some(held) = held else {
            return Ok(());
        };
        for rule in &mut script.rules {
            for set in rule.action.add.iter_mut().chain(rule.action.remove.iter_mut()) {
                if let MailSet::Mailbox(id) = set
                    && let Some((_, to)) = moved.iter().find(|(from, _)| from == id)
                {
                    *id = to.clone();
                }
            }
        }
        self.turn_off_orphans(&mut script, &ext, None).await?;
        self.drop_server_reply(&mut script);
        let text = script::write(&script, &self.address, &|set| self.folder(set), &ext).map_err(|e| self.words(e))?;
        if text != held {
            self.api.put(SCRIPT_NAME, &text).await.map_err(|e| self.err(e))?;
        }
        Ok(())
    }

    /// Lists a JMAP account's mailboxes when nothing has yet, or again when
    /// `fresh`, so a rule saved before the first listing waits for it
    /// rather than failing on a folder the adapter has not seen.
    async fn know_folders(&self, fresh: bool) -> Result<(), BackendError> {
        match &self.mail {
            AnyMail::Jmap(_) if fresh => self.mail.mailboxes().await.map(drop),
            AnyMail::Jmap(jmap) => jmap.ensure_listed().await,
            _ => Ok(()),
        }
    }

    /// The rule files into a folder the account no longer lists.
    fn misses_folder(&self, rule: &Filter, ext: &Extensions) -> bool {
        let alone = Script { rules: vec![Filter { off: false, ..rule.clone() }], ..Script::default() };
        matches!(script::write(&alone, &self.address, &|set| self.folder(set), ext), Err(WriteError::NoFolder(_)))
    }

    /// Turns off each rule but `keep` whose folder a JMAP account no longer
    /// lists, after one fresh listing confirms it is gone. Only a JMAP
    /// account can tell: an IMAP folder's id is its name, which the script
    /// names whether or not the folder is there.
    async fn turn_off_orphans(&self, script: &mut Script, ext: &Extensions, keep: Option<&str>) -> Result<(), BackendError> {
        if !matches!(self.mail, AnyMail::Jmap(_)) {
            return Ok(());
        }
        self.know_folders(false).await?;
        let orphaned = |rules: &Script| -> Vec<usize> {
            rules
                .rules
                .iter()
                .enumerate()
                .filter(|(_, r)| !r.off && (keep.is_none() || r.id.as_deref() != keep) && self.misses_folder(r, ext))
                .map(|(i, _)| i)
                .collect()
        };
        if orphaned(script).is_empty() {
            return Ok(());
        }
        self.know_folders(true).await?;
        for at in orphaned(script) {
            script.rules[at].off = true;
        }
        Ok(())
    }

    /// The Sieve extensions the server offered when last asked.
    pub fn extensions(&self) -> Option<Extensions> {
        self.state().extensions.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn err(&self, err: SieveError) -> BackendError {
        match err {
            SieveError::Network(detail) => BackendError::Offline(detail),
            SieveError::Auth(_) => BackendError::NeedsReauth,
            SieveError::NotFound => BackendError::NotFound,
            SieveError::Refused(words) => BackendError::Refused(words),
            SieveError::NoStartTls => BackendError::Refused(fill(
                &gettext(
                    "{provider} offers no encrypted connection for rules, so Penguin Mail does not send your password there.",
                ),
                &[("provider", &self.provider)],
            )),
            other => BackendError::Refused(other.to_string()),
        }
    }

    fn words(&self, err: WriteError) -> BackendError {
        let provider = [("provider", self.provider.as_str())];
        let text = match err {
            WriteError::Needs(extension) => fill(
                &gettext(
                    "{provider} cannot run this rule: its server lacks the Sieve extension “{extension}”.",
                ),
                &[("provider", &self.provider), ("extension", extension)],
            ),
            WriteError::NoFolder(_) => {
                gettext("The folder this rule files into is not on the server yet.")
            }
            WriteError::Unsayable(Unsayable::NeverSpam) => fill(
                &gettext("A rule on {provider} cannot keep mail out of Spam."),
                &provider,
            ),
            WriteError::Unsayable(Unsayable::Importance) => fill(
                &gettext("{provider} does not mark mail as important."),
                &provider,
            ),
            WriteError::Unsayable(Unsayable::TwoFolders) => {
                gettext("A rule can file mail into one folder.")
            }
            WriteError::Unsayable(Unsayable::NothingToMatch) => {
                gettext("Say which mail the rule is for")
            }
            WriteError::Unsayable(Unsayable::NothingToDo) => gettext("Choose what the rule does"),
        };
        BackendError::Refused(text)
    }

    /// The UTF-8 name of the folder a set files into: an IMAP mailbox's
    /// decoded path, or a JMAP mailbox's path, `Parent/Child`, since a
    /// JMAP id names nothing a Sieve script can file into.
    fn folder(&self, set: &MailSet) -> Option<String> {
        let id = match set {
            MailSet::Role(role) => self.mail.mailbox_for(*role)?,
            MailSet::Mailbox(id) => id.clone(),
            _ => return None,
        };
        match &self.mail {
            AnyMail::Jmap(jmap) => jmap.mailbox_path(&id),
            _ => Some(mailrs_imap::utf7::decode(&id)),
        }
    }

    async fn load(&self) -> Result<(Script, Extensions, Running), BackendError> {
        let (script, ext, running, _) = self.load_text().await?;
        Ok((script, ext, running))
    }

    /// As `load`, with the script's text as the server holds it, `None`
    /// when the server has no script of the app's.
    async fn load_text(&self) -> Result<(Script, Extensions, Running, Option<String>), BackendError> {
        let caps = self.api.capabilities().await.map_err(|e| self.err(e))?;
        self.state().extensions = Some(caps.sieve.clone());
        let listed = self.api.scripts().await.map_err(|e| self.err(e))?;
        let text = match listed.iter().any(|l| l.name == SCRIPT_NAME) {
            true => Some(self.api.get(SCRIPT_NAME).await.map_err(|e| self.err(e))?),
            false => None,
        };
        let script = text.as_deref().map(script::read).unwrap_or_default();
        let running = match listed.iter().find(|l| l.active) {
            Some(l) if l.name == SCRIPT_NAME => Running::Ours,
            Some(l) => Running::Theirs(l.name.clone()),
            None => Running::Nothing,
        };
        Ok((script, caps.sieve, running, text))
    }

    /// Writes `script` and makes it the running one. `keep` names the rule
    /// being saved, whose missing folder is the person's to hear about;
    /// any other rule whose folder is gone is turned off.
    async fn store(
        &self,
        mut script: Script,
        ext: &Extensions,
        running: Running,
        keep: Option<&str>,
    ) -> Result<(), BackendError> {
        self.turn_off_orphans(&mut script, ext, keep).await?;
        self.drop_server_reply(&mut script);
        if let Running::Theirs(name) = &running {
            if ext.has("include") && !self.is_server_reply(name) {
                script.include = Some(name.clone());
            } else if !self.state().take_over {
                return Err(BackendError::WouldReplace {
                    script: name.clone(),
                });
            }
        }
        let text = script::write(&script, &self.address, &|set| self.folder(set), ext)
            .map_err(|e| self.words(e))?;
        self.api
            .put(SCRIPT_NAME, &text)
            .await
            .map_err(|e| self.err(e))?;
        if !matches!(running, Running::Ours) {
            self.api
                .activate(SCRIPT_NAME)
                .await
                .map_err(|e| self.err(e))?;
        }
        Ok(())
    }

    /// The script holds only rules of Penguin Mail's own, so it cannot
    /// keep one the app marked read-only.
    fn refuse_read_only(filter: &Filter) -> Result<(), BackendError> {
        if filter.read_only {
            return Err(BackendError::Refused(gettext("The rule is read-only.")));
        }
        Ok(())
    }

    /// `filter` as the person saved it: with an id, and on, since saving a
    /// rule that is off turns it back on.
    fn fresh(filter: &Filter) -> Filter {
        Filter {
            id: Some(filter.id.clone().unwrap_or_else(|| format!("sieve-{:016x}", rand::random::<u64>()))),
            off: false,
            ..filter.clone()
        }
    }
}

impl<M: ManageSieveApi> RulesService for SieveRules<M> {
    async fn filters(&self) -> Result<Vec<Filter>, BackendError> {
        Ok(self.load().await?.0.rules)
    }

    async fn create_filter(&self, filter: &Filter) -> Result<Filter, BackendError> {
        Self::refuse_read_only(filter)?;
        let _writing = self.writing.lock().await;
        let (mut script, ext, running) = self.load().await?;
        let made = Self::fresh(filter);
        script.rules.push(made.clone());
        self.store(script, &ext, running, made.id.as_deref()).await?;
        Ok(made)
    }

    async fn delete_filter(&self, id: &str) -> Result<(), BackendError> {
        let _writing = self.writing.lock().await;
        let (mut script, ext, running) = self.load().await?;
        let before = script.rules.len();
        script.rules.retain(|r| r.id.as_deref() != Some(id));
        if script.rules.len() == before {
            return Err(BackendError::NotFound);
        }
        self.store(script, &ext, running, None).await
    }

    async fn replace_filter(&self, old_id: &str, new: &Filter) -> Result<Filter, BackendError> {
        Self::refuse_read_only(new)?;
        let _writing = self.writing.lock().await;
        let (mut script, ext, running) = self.load().await?;
        let slot = script
            .rules
            .iter()
            .position(|r| r.id.as_deref() == Some(old_id))
            .ok_or(BackendError::NotFound)?;
        let made = Self::fresh(new);
        script.rules[slot] = made.clone();
        self.store(script, &ext, running, made.id.as_deref()).await?;
        Ok(made)
    }

    async fn take_over(&self) {
        self.state().take_over = true;
    }

    fn queues_offline(&self) -> bool {
        true
    }
}

impl<M: ManageSieveApi> AutoReplyService for SieveRules<M> {
    async fn vacation(&self) -> Result<Vacation, BackendError> {
        Ok(self.load().await?.0.vacation.unwrap_or_default())
    }

    async fn set_vacation(&self, vacation: &Vacation) -> Result<(), BackendError> {
        let _writing = self.writing.lock().await;
        let (mut script, ext, running) = self.load().await?;
        script.vacation = Some(vacation.clone());
        self.store(script, &ext, running, None).await
    }

    fn keeps_subject(&self) -> bool {
        true
    }

    fn limits_to_contacts(&self) -> bool {
        false
    }
}
