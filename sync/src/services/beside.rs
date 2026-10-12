//! Rules on a JMAP server that runs Sieve, beside the local rules of an
//! account switched from IMAP. The switch moves the local rules into the
//! app's script; where the server could not take them (the person runs a
//! script of their own that the server cannot include), they stay in the
//! store and run on this computer as before. The Rules dialog lists both,
//! a change to a rule kept here stays here, and a new rule goes to the
//! server.

use mailrs_domain::Filter;

use super::{AnyJmap, JmapSieve, LocalRules, RulesService, SieveRules};
use crate::BackendError;

#[derive(Clone)]
pub struct RulesBeside {
    server: SieveRules<JmapSieve<AnyJmap>>,
    local: LocalRules,
}

impl RulesBeside {
    pub fn new(server: SieveRules<JmapSieve<AnyJmap>>, local: LocalRules) -> RulesBeside {
        RulesBeside { server, local }
    }

    pub fn server(&self) -> &SieveRules<JmapSieve<AnyJmap>> {
        &self.server
    }

    pub fn local(&self) -> &LocalRules {
        &self.local
    }

    /// The ids of the rules that run on this computer.
    pub async fn kept_here(&self) -> Result<Vec<String>, BackendError> {
        Ok(self.local.filters().await?.into_iter().filter_map(|f| f.id).collect())
    }

    async fn is_here(&self, id: &str) -> Result<bool, BackendError> {
        Ok(self.kept_here().await?.iter().any(|here| here == id))
    }

    /// Moves the rules kept here into the server's script. The store lets
    /// go of them only once the script holds them; a failure in between
    /// leaves them in both, and the next move adds none twice.
    pub async fn move_in(&self) -> Result<(), BackendError> {
        let here = self.local.filters().await?;
        if here.is_empty() {
            return Ok(());
        }
        self.server.take_in(&here).await?;
        for rule in here.iter().filter_map(|r| r.id.as_deref()) {
            match self.local.delete_filter(rule).await {
                Ok(()) | Err(BackendError::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// The server's listing with the rules kept here after it.
    pub async fn listing(&self) -> Result<(Vec<Filter>, Vec<String>), BackendError> {
        let (mut rules, elsewhere) = self.server.listing().await?;
        rules.extend(self.local.filters().await?);
        Ok((rules, elsewhere))
    }
}

impl RulesService for RulesBeside {
    async fn filters(&self) -> Result<Vec<Filter>, BackendError> {
        let mut rules = self.server.filters().await?;
        rules.extend(self.local.filters().await?);
        Ok(rules)
    }

    async fn create_filter(&self, filter: &Filter) -> Result<Filter, BackendError> {
        self.server.create_filter(filter).await
    }

    async fn delete_filter(&self, id: &str) -> Result<(), BackendError> {
        match self.is_here(id).await? {
            true => self.local.delete_filter(id).await,
            false => self.server.delete_filter(id).await,
        }
    }

    async fn replace_filter(&self, old_id: &str, new: &Filter) -> Result<Filter, BackendError> {
        match self.is_here(old_id).await? {
            true => self.local.replace_filter(old_id, new).await,
            false => self.server.replace_filter(old_id, new).await,
        }
    }

    async fn take_over(&self) {
        self.server.take_over().await;
    }

    fn queues_offline(&self) -> bool {
        self.server.queues_offline()
    }
}
