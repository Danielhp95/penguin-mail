//! Where an address's JMAP session is: the provider table by domain or by
//! MX, then the `_jmap._tcp` SRV record (RFC 8620 section 2.2), then the
//! domain's own `/.well-known/jmap`. The session needs credentials, so
//! this only says where to sign in; a well-known URL that answers at all
//! (401 included) says a JMAP server is there. Only the domain goes out.

use crate::find::STEP_LIMIT;
use crate::name::{domain_of, host, is_within};
use crate::table::Table;
use crate::{Net, Source, SrvRecord};

/// A JMAP session to sign in to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JmapFound {
    pub session_url: String,
    pub source: Source,
    /// The host is outside the address's domain and did not come from the
    /// table, so the person says yes before a password goes there.
    pub confirm: bool,
    pub provider: Option<JmapProvider>,
}

/// What the table knows of a provider's JMAP sign-in beyond the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JmapProvider {
    pub name: String,
    pub token_url: Option<String>,
    /// The plan a token needs more than, in the provider's own words.
    pub token_plan_above: Option<String>,
    /// The access to tick when making a token, in the provider's words.
    pub token_access: Vec<String>,
    /// Capabilities a token never carries, such as Fastmail's automatic
    /// reply. Words only; features follow the session.
    pub token_lacks: Vec<String>,
    /// The scopes a sign-in in the browser asks for. Empty when the table
    /// names none, and the server's metadata then decides.
    pub oauth_scopes: Vec<String>,
    /// The provider's IMAP host, which finds its CalDAV and CardDAV
    /// servers in the table.
    pub imap_host: Option<String>,
}

pub async fn find_jmap<N: Net>(net: &N, address: &str) -> Option<JmapFound> {
    let domain = domain_of(address)?;
    let table = Table::built_in();
    let listed = |entry: &crate::table::Entry, source| {
        let jmap = entry.jmap.as_ref()?;
        Some(JmapFound { session_url: jmap.session.clone(), source, confirm: false, provider: entry.jmap_provider() })
    };
    if let Some(entry) = table.by_domain(&domain) {
        return listed(entry, Source::Table);
    }
    let hosts = tokio::time::timeout(STEP_LIMIT, net.mx(&domain)).await.unwrap_or_default();
    if let Some(entry) = table.by_mx(&hosts) {
        return listed(entry, Source::Mx);
    }
    let records = tokio::time::timeout(STEP_LIMIT, net.srv(&format!("_jmap._tcp.{domain}"))).await.unwrap_or_default();
    if records.iter().any(|r| r.target.trim_end_matches('.').is_empty()) {
        // A target of "." says the domain offers no JMAP.
        return None;
    }
    if let Some(found) = from_srv(net, records, &domain).await {
        return Some(found);
    }
    let url = format!("https://{domain}/.well-known/jmap");
    let target = tokio::time::timeout(STEP_LIMIT, net.locate(&url)).await.ok().flatten()?;
    let confirm = leaves_domain(&target, &domain);
    Some(JmapFound { session_url: target, source: Source::WellKnown, confirm, provider: None })
}

/// Whether `url` names a host outside `domain`. An unparseable URL counts
/// as outside, so the person is asked.
fn leaves_domain(url: &str, domain: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_none_or(|h| !is_within(&h, domain))
}

async fn from_srv<N: Net>(net: &N, mut records: Vec<SrvRecord>, domain: &str) -> Option<JmapFound> {
    records.sort_by(|a, b| a.priority.cmp(&b.priority).then(b.weight.cmp(&a.weight)));
    let best = records.into_iter().find(|r| r.port != 0)?;
    let target = host(&best.target)?;
    let port = if best.port == 443 { String::new() } else { format!(":{}", best.port) };
    let well_known = format!("https://{target}{port}/.well-known/jmap");
    // The session lives where the well-known URL leads; when it answers
    // nothing now, the sign-in tries the well-known URL itself.
    let session_url = tokio::time::timeout(STEP_LIMIT, net.locate(&well_known)).await.ok().flatten().unwrap_or(well_known);
    let confirm = !is_within(&target, domain) || leaves_domain(&session_url, domain);
    Some(JmapFound { session_url, source: Source::Srv, confirm, provider: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeNet;
    use crate::jmap_named;

    #[tokio::test]
    async fn fastmail_comes_from_the_table_and_asks_nobody() {
        let net = FakeNet::default();
        let found = find_jmap(&net, "me@fastmail.com").await.unwrap();
        assert_eq!(found.session_url, "https://api.fastmail.com/jmap/session");
        assert_eq!(found.source, Source::Table);
        assert!(!found.confirm);
        let provider = found.provider.unwrap();
        assert_eq!(provider.name, "Fastmail");
        assert_eq!(provider.token_plan_above.as_deref(), Some("Basic"));
        assert_eq!(provider.token_access, ["Email", "Email submission", "Contacts", "Masked Email"]);
        assert_eq!(provider.imap_host.as_deref(), Some("imap.fastmail.com"));
        assert!(net.requests().is_empty());
    }

    #[tokio::test]
    async fn a_custom_domain_on_fastmail_is_found_by_its_mx() {
        let net = FakeNet::default().answer_mx("example.org", &["in1-smtp.messagingengine.com"]);
        let found = find_jmap(&net, "me@example.org").await.unwrap();
        assert_eq!(found.session_url, "https://api.fastmail.com/jmap/session");
        assert_eq!(found.source, Source::Mx);
        assert!(!found.confirm, "the table names Fastmail's session URL");
    }

    #[tokio::test]
    async fn srv_names_the_host_and_the_well_known_path_follows_its_redirect() {
        let net = FakeNet::default()
            .answer_srv(
                "_jmap._tcp.example.org",
                vec![SrvRecord { priority: 0, weight: 1, port: 443, target: "mail.example.org.".into() }],
            )
            .locate_at("https://mail.example.org/.well-known/jmap", "https://mail.example.org/jmap/session");
        let found = find_jmap(&net, "me@example.org").await.unwrap();
        assert_eq!(found.session_url, "https://mail.example.org/jmap/session");
        assert_eq!(found.source, Source::Srv);
        assert!(!found.confirm);
    }

    #[tokio::test]
    async fn an_srv_target_outside_the_domain_needs_a_yes() {
        let net = FakeNet::default().answer_srv(
            "_jmap._tcp.example.org",
            vec![SrvRecord { priority: 0, weight: 1, port: 8443, target: "jmap.hoster.net.".into() }],
        );
        let found = find_jmap(&net, "me@example.org").await.unwrap();
        assert_eq!(found.session_url, "https://jmap.hoster.net:8443/.well-known/jmap");
        assert!(found.confirm);
    }

    #[tokio::test]
    async fn an_in_domain_srv_target_that_redirects_off_domain_needs_a_yes() {
        let net = FakeNet::default()
            .answer_srv(
                "_jmap._tcp.example.org",
                vec![SrvRecord { priority: 0, weight: 1, port: 443, target: "mail.example.org.".into() }],
            )
            .locate_at("https://mail.example.org/.well-known/jmap", "https://other.net/session");
        let found = find_jmap(&net, "me@example.org").await.unwrap();
        assert_eq!(found.session_url, "https://other.net/session");
        assert!(found.confirm);
    }

    #[tokio::test]
    async fn the_domains_well_known_url_is_tried_last() {
        let net = FakeNet::default().locate_at("https://example.org/.well-known/jmap", "https://example.org/jmap/session");
        let found = find_jmap(&net, "me@example.org").await.unwrap();
        assert_eq!(found.session_url, "https://example.org/jmap/session");
        assert_eq!(found.source, Source::WellKnown);
        assert!(!found.confirm);
    }

    #[tokio::test]
    async fn a_well_known_redirect_to_another_domain_needs_a_yes() {
        let net = FakeNet::default().locate_at("https://example.org/.well-known/jmap", "https://jmap.hoster.net/session");
        assert!(find_jmap(&net, "me@example.org").await.unwrap().confirm);
    }

    #[tokio::test]
    async fn an_srv_dot_target_says_there_is_no_jmap() {
        let net = FakeNet::default()
            .answer_srv("_jmap._tcp.example.org", vec![SrvRecord { priority: 0, weight: 0, port: 0, target: ".".into() }])
            .locate_at("https://example.org/.well-known/jmap", "https://example.org/jmap/session");
        assert_eq!(find_jmap(&net, "me@example.org").await, None);
    }

    #[tokio::test]
    async fn nothing_found_is_none_and_only_the_domain_went_out() {
        let net = FakeNet::default();
        assert_eq!(find_jmap(&net, "me@example.org").await, None);
        for request in net.requests() {
            let name = request.name();
            assert!(!name.contains("me@"), "the address went out in {name}");
        }
    }

    #[test]
    fn the_table_names_fastmail_s_jmap_facts_by_provider_name() {
        let fastmail = jmap_named("Fastmail").unwrap();
        assert_eq!(fastmail.token_lacks, ["urn:ietf:params:jmap:vacationresponse"]);
        assert!(jmap_named("iCloud Mail").is_none());
    }

    #[test]
    fn the_table_names_the_scopes_a_fastmail_sign_in_asks_for() {
        let fastmail = jmap_named("Fastmail").unwrap();
        assert!(fastmail.oauth_scopes.contains(&"offline_access".to_string()));
        assert!(fastmail.oauth_scopes.contains(&"urn:ietf:params:oauth:scope:mail".to_string()));
    }

    #[test]
    fn the_table_names_fastmail_s_session_by_provider_name() {
        assert_eq!(crate::jmap_session_named("Fastmail").as_deref(), Some("https://api.fastmail.com/jmap/session"));
        assert!(crate::jmap_session_named("iCloud Mail").is_none());
    }
}
