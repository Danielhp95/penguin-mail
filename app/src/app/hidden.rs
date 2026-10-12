//! Hide My Email addresses kept in the settings file. `mailrs_sync`'s
//! `AccountSettings` makes the Gmail filters behind each one and hands
//! back the address to keep; this module finds the account an address
//! belongs to and keeps the list. The dialog and the assistant both come
//! here. An account whose server makes masked addresses, as Fastmail
//! does, keeps them on the server, so those calls go straight to it.

use std::rc::Rc;

use anyhow::{anyhow, bail};
use mailrs_domain::AccountId;
use mailrs_sync::Permitted;
use mailrs_sync::hidden::{self, HiddenAddress};
use mailrs_sync::masked::{MaskedAddress, MaskedState};

use super::App;
use crate::settings::Change;

impl App {
    pub fn hidden_addresses(&self) -> Vec<HiddenAddress> {
        self.settings.borrow().hidden_addresses.clone()
    }

    /// Makes a new alias of `account_id`, gives it its Gmail filters, and
    /// saves it.
    pub async fn create_hidden_address(
        self: &Rc<Self>,
        account_id: AccountId,
        note: &str,
    ) -> anyhow::Result<Permitted<HiddenAddress>> {
        let account = self
            .accounts()
            .into_iter()
            .find(|a| a.id == account_id)
            .ok_or_else(|| anyhow!("that account is not connected"))?;
        if self.core.account(account_id).is_none() {
            bail!("{} is not syncing yet", account.email);
        }
        let (settings, taken) = (self.core.gmail_settings(), self.hidden_addresses());
        let note = note.to_string();
        let made = self
            .core
            .call(async move {
                settings
                    .create_hidden_address(account_id, &account.email, &note, &taken)
                    .await
            })
            .await?;
        if let Permitted::Done(hidden) = &made {
            self.change_settings(Change::SaveHiddenAddress(hidden.clone()));
        }
        Ok(made)
    }

    /// Turns an alias on or off. Off sends its mail to the Trash.
    pub async fn set_hidden_address_active(
        self: &Rc<Self>,
        address: &str,
        active: bool,
    ) -> anyhow::Result<Permitted<()>> {
        let (hidden, account_id) = self.hidden_address(address)?;
        let settings = self.core.gmail_settings();
        let changed = self
            .core
            .call(async move {
                settings
                    .set_hidden_address_active(account_id, &hidden, active)
                    .await
            })
            .await?;
        let Permitted::Done(changed) = changed else {
            return Ok(Permitted::NeedsPermission);
        };
        self.change_settings(Change::SaveHiddenAddress(changed));
        Ok(Permitted::Done(()))
    }

    /// Removes an alias and its Gmail filters. Mail to it then arrives like
    /// any other mail.
    pub async fn delete_hidden_address(
        self: &Rc<Self>,
        address: &str,
    ) -> anyhow::Result<Permitted<()>> {
        let (hidden, account_id) = self.hidden_address(address)?;
        let settings = self.core.gmail_settings();
        let gone = hidden.address.clone();
        let dropped = self
            .core
            .call(async move { settings.delete_hidden_address(account_id, &hidden).await })
            .await?;
        if dropped == Permitted::Done(()) {
            self.change_settings(Change::ForgetHiddenAddress(gone));
        }
        Ok(dropped)
    }

    /// Every masked address of `account_id`, from its server.
    pub async fn masked_addresses(self: &Rc<Self>, account_id: AccountId) -> anyhow::Result<Vec<MaskedAddress>> {
        let settings = self.core.gmail_settings();
        self.core.call(async move { settings.masked_addresses(account_id).await }).await
    }

    /// A new masked address of `account_id` for `site`, with `note` saying
    /// where it was given out.
    pub async fn create_masked_address(
        self: &Rc<Self>,
        account_id: AccountId,
        site: &str,
        note: &str,
    ) -> anyhow::Result<MaskedAddress> {
        let (settings, site, note) = (self.core.gmail_settings(), site.to_string(), note.to_string());
        self.core.call(async move { settings.create_masked_address(account_id, &site, &note).await }).await
    }

    /// Turns a masked address on or off, or deletes it.
    pub async fn set_masked_state(
        self: &Rc<Self>,
        account_id: AccountId,
        id: &str,
        state: MaskedState,
    ) -> anyhow::Result<()> {
        let (settings, id) = (self.core.gmail_settings(), id.to_string());
        self.core.call(async move { settings.set_masked_state(account_id, &id, state).await }).await
    }

    /// The kept alias `address`, and the connected account it belongs to.
    fn hidden_address(&self, address: &str) -> anyhow::Result<(HiddenAddress, AccountId)> {
        let kept = self.hidden_addresses();
        let hidden = hidden::find(&kept, address)
            .cloned()
            .ok_or_else(|| anyhow!("{address} is not a Hide My Email address"))?;
        let account_id = self
            .accounts()
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(&hidden.account))
            .map(|a| a.id)
            .filter(|id| self.core.account(*id).is_some())
            .ok_or_else(|| anyhow!("{} is not connected", hidden.account))?;
        Ok((hidden, account_id))
    }
}
