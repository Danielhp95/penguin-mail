//! JMAP servers in Docker: Cyrus, which Fastmail runs; Stalwart; and
//! Apache James. Each serves JMAP over plain HTTP on 127.0.0.1, which the
//! client takes only on this computer. Each server writes its own address
//! into the URLs the session hands out, so the host port is picked before
//! the container starts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{Container, REQUIRE_JMAP};

/// Cyrus's own test image: JMAP on 8080, users `user1` to `user5`, any
/// password (fakesaslauthd). Pinned by digest.
pub const CYRUS_IMAGE: &str = "ghcr.io/cyrusimap/cyrus-docker-test-server:bookworm@sha256:035aa4ffdb84df95a1057645d05c9a156dde74e5787ef1a11f7d9c244a38b2c0";

/// Stalwart 0.16.25: HTTP on 8080, an administrator from
/// `STALWART_RECOVERY_ADMIN`, bootstrap mode on first start.
pub const STALWART_IMAGE: &str = "stalwartlabs/stalwart:v0.16.25@sha256:74e5a7d55303ba525d939c6bf97ed4e010df7521f52d80afc22a815b66bd53f3";

/// Apache James 3.9.1 on its in-memory store: JMAP on 80, a domain from
/// `DOMAIN`, users from `james-cli`.
pub const JAMES_IMAGE: &str = "apache/james:memory-3.9.1@sha256:093b35e8e11ab63b379799d42cf4a1b095ba43f49da28de5c729c9e848b52e8e";

/// Stalwart's recovery administrator, and the domain its bootstrap makes.
const STALWART_ADMIN: &str = "admin";
const STALWART_DOMAIN: &str = "stalwart.test";

/// James starts a JVM and takes longer than the others.
const READY_WITHIN: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JmapKind {
    Cyrus,
    Stalwart,
    James,
}

impl JmapKind {
    pub const ALL: [JmapKind; 3] = [JmapKind::Cyrus, JmapKind::Stalwart, JmapKind::James];
}

pub struct JmapServer {
    container: Container,
    /// One pool for every call the test makes as the other client. It does
    /// not get past Cyrus 3.13 while an EventSource stream is open: Cyrus
    /// then answers no request on any connection, new or kept alive, until
    /// the stream closes.
    http: reqwest::Client,
    pub kind: JmapKind,
    pub session_url: String,
    /// The user name Basic sends.
    pub user: String,
    pub address: String,
}

/// A port nothing listens on now, for a server that must know its own
/// address before it starts. Another process could take it in between;
/// the start then fails and the test skips.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free port on 127.0.0.1")
}

/// Whether `url` answers anything but a server error, a connection refusal
/// or a reset, before the clock runs out. With `login`, it must answer 200
/// to that user: Cyrus refuses a login for a second or so after it starts
/// answering.
async fn answers(url: &str, login: Option<(&str, &str)>) -> bool {
    let http = reqwest::Client::new();
    let deadline = Instant::now() + READY_WITHIN;
    while Instant::now() < deadline {
        let mut request = http.get(url).timeout(Duration::from_secs(5));
        if let Some((user, password)) = login {
            request = request.basic_auth(user, Some(password));
        }
        if let Ok(response) = request.send().await {
            let status = response.status();
            let ready = match login {
                Some(_) => status.is_success(),
                None => status.as_u16() < 500,
            };
            if ready {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// A throwaway PKCS12 keystore for James in `dir`. The image ships none,
/// and its IMAP, SMTP, POP3 and ManageSieve servers refuse to start without
/// one; its configuration opens it with `james72laBalle`. JMAP itself stays
/// plain HTTP.
fn james_keystore(dir: &Path) -> Option<PathBuf> {
    let steps: [&[&str]; 2] = [
        &[
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-nodes",
            "-keyout",
            "james.key",
            "-out",
            "james.crt",
            "-subj",
            "/CN=localhost",
            "-days",
            "2",
        ],
        &[
            "pkcs12",
            "-export",
            "-name",
            "james",
            "-inkey",
            "james.key",
            "-in",
            "james.crt",
            "-out",
            "keystore",
            "-passout",
            "pass:james72laBalle",
        ],
    ];
    for args in steps {
        let Ok(out) = Command::new("openssl")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
        else {
            return crate::unavailable_under(REQUIRE_JMAP, "openssl is not installed");
        };
        assert!(
            out.status.success(),
            "openssl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Some(dir.join("keystore"))
}

/// A URL from the session, with `{accountId}` filled in, resolved against
/// the session URL: Stalwart in bootstrap mode hands out relative ones.
fn endpoint(session_url: &str, template: &str, account: &str) -> reqwest::Url {
    reqwest::Url::parse(session_url)
        .and_then(|url| url.join(&template.replace("{accountId}", account)))
        .unwrap_or_else(|_| panic!("{template} does not resolve against {session_url}"))
}

/// Whether Cyrus's start script has made its users. Cyrus answers before
/// the script is done, then `make5.pl` deletes and remakes `user1` to
/// `user5` and grants each its rights mailbox by mailbox, so an early write
/// can land in a mailbox about to go or be refused as `invalidProperties`.
/// A `user5` left from an earlier pass can look ready before the script
/// deletes it, so the wait is for what the script starts once `make5.pl`
/// has returned: the user API on `web_url`. `user5`'s mailboxes are then
/// checked as well.
async fn cyrus_users_made(session_url: &str, web_url: &str, password: &str) -> bool {
    if !answers(web_url, None).await {
        return false;
    }
    let http = reqwest::Client::new();
    let deadline = Instant::now() + READY_WITHIN;
    while Instant::now() < deadline {
        if every_mailbox_takes_mail(&http, session_url, "user5", password).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn every_mailbox_takes_mail(http: &reqwest::Client, session_url: &str, user: &str, password: &str) -> bool {
    let session: Option<Value> = match http.get(session_url).basic_auth(user, Some(password)).send().await {
        Ok(response) if response.status().is_success() => response.json().await.ok(),
        _ => None,
    };
    let Some(session) = session else { return false };
    let (Some(account), Some(api)) = (
        session["primaryAccounts"]["urn:ietf:params:jmap:mail"].as_str(),
        session["apiUrl"].as_str(),
    ) else {
        return false;
    };
    let calls = json!({
        "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
        "methodCalls": [["Mailbox/get", {"accountId": account, "ids": null, "properties": ["role", "myRights"]}, "0"]]
    });
    let answer: Option<Value> = match http.post(endpoint(session_url, api, "")).basic_auth(user, Some(password)).json(&calls).send().await {
        Ok(response) if response.status().is_success() => response.json().await.ok(),
        _ => None,
    };
    let Some(list) = answer.as_ref().and_then(|a| a["methodResponses"][0][1]["list"].as_array()) else {
        return false;
    };
    list.iter().any(|m| m["role"] == "inbox") && list.iter().all(|m| m["myRights"]["mayAddItems"] == true)
}

/// One message over SMTP or LMTP to 127.0.0.1 inside a container, in Perl
/// with core modules only. The recipient is the first argument, the
/// message comes on stdin, and any refusal ends it with a non-zero status.
const DELIVER_SCRIPT: &str = r#"
use IO::Socket::INET;
my ($rcpt) = @ARGV;
my $msg = do { local $/; <STDIN> };
my $s = IO::Socket::INET->new(PeerAddr => '127.0.0.1', PeerPort => {port}) or die "no connection";
$s->autoflush(1);
sub said { my $l; do { $l = <$s>; } while (defined $l && $l =~ /^\d{3}-/); die "refused: $l" unless defined $l && $l =~ /^[23]/; }
said();
print $s "{greeting} client.example.test\r\n"; said();
print $s "MAIL FROM:<pest\@example.test>\r\n"; said();
print $s "RCPT TO:<$rcpt>\r\n"; said();
print $s "DATA\r\n"; said();
$msg =~ s/^\./../mg;
print $s $msg, "\r\n.\r\n"; said();
print $s "QUIT\r\n";
"#;

impl JmapServer {
    pub async fn start(kind: JmapKind, password: &str) -> Option<JmapServer> {
        match kind {
            JmapKind::Cyrus => Self::cyrus(password).await,
            JmapKind::Stalwart => Self::stalwart(password).await,
            JmapKind::James => Self::james(password).await,
        }
    }

    pub fn id(&self) -> &str {
        self.container.id()
    }

    async fn cyrus(password: &str) -> Option<JmapServer> {
        let port = free_port();
        let web = std::iter::repeat_with(free_port).find(|p| *p != port).unwrap_or_default();
        let base = format!("http://localhost:{port}");
        let options = vec![
            "-p".to_string(),
            format!("127.0.0.1:{port}:8080"),
            "-p".into(),
            format!("127.0.0.1:{web}:8001"),
            "-e".into(),
            format!("JMAPBASEURL={base}"),
            "-e".into(),
            "DEFAULTDOMAIN=example.com".into(),
        ];
        let container = Container::start(REQUIRE_JMAP, &options, CYRUS_IMAGE, &[], &[]).await?;
        // Cyrus serves the session at /jmap; /.well-known/jmap redirects
        // there.
        let session_url = format!("{base}/jmap");
        if !answers(&session_url, Some(("user1", password))).await
            || !cyrus_users_made(&session_url, &format!("http://localhost:{web}/"), password).await
        {
            return container
                .not_ready(REQUIRE_JMAP, "Cyrus", READY_WITHIN)
                .await;
        }
        Some(JmapServer {
            container,
            http: reqwest::Client::new(),
            kind: JmapKind::Cyrus,
            session_url,
            user: "user1".into(),
            address: "user1@example.com".into(),
        })
    }

    /// Stalwart 0.16.25 has no configuration file, so it starts in
    /// bootstrap mode, where it refuses to make a domain or a user. The
    /// starter finishes the bootstrap as the recovery administrator,
    /// restarts the container to leave that mode, and then makes the user
    /// in the domain the bootstrap made.
    async fn stalwart(password: &str) -> Option<JmapServer> {
        let port = free_port();
        let base = format!("http://localhost:{port}");
        let session_url = format!("{base}/jmap/session");
        let options = vec![
            "-p".to_string(),
            format!("127.0.0.1:{port}:8080"),
            "-e".into(),
            format!("STALWART_RECOVERY_ADMIN={STALWART_ADMIN}:{password}"),
            "-e".into(),
            format!("STALWART_PUBLIC_URL={base}"),
        ];
        let container = Container::start(REQUIRE_JMAP, &options, STALWART_IMAGE, &[], &[]).await?;
        if !answers(&session_url, Some((STALWART_ADMIN, password))).await {
            return container
                .not_ready(REQUIRE_JMAP, "Stalwart", READY_WITHIN)
                .await;
        }
        let server = JmapServer {
            container,
            http: reqwest::Client::new(),
            kind: JmapKind::Stalwart,
            session_url,
            user: format!("me@{STALWART_DOMAIN}"),
            address: format!("me@{STALWART_DOMAIN}"),
        };
        // It refuses the container's hex hostname, so the bootstrap names
        // one. No certificate or DKIM key: the test serves plain HTTP and
        // sends no mail.
        let booted = server
            .stalwart_admin(
                password,
                json!([["x:Bootstrap/set", {"update": {"singleton": {
                    "serverHostname": format!("mail.{STALWART_DOMAIN}"),
                    "defaultDomain": STALWART_DOMAIN,
                    "requestTlsCertificate": false,
                    "generateDkimKeys": false
                }}}, "0"]]),
            )
            .await;
        assert!(
            booted["methodResponses"][0][1]["updated"]["singleton"].is_object(),
            "Stalwart did not finish its bootstrap: {booted}"
        );
        server.container.restart().await;
        if !answers(&server.session_url, Some((STALWART_ADMIN, password))).await {
            return server
                .container
                .not_ready(REQUIRE_JMAP, "Stalwart after its restart", READY_WITHIN)
                .await;
        }
        let domains = server
            .stalwart_admin(password, json!([["x:Domain/get", {"ids": null}, "0"]]))
            .await;
        let domain = domains["methodResponses"][0][1]["list"]
            .as_array()
            .and_then(|list| list.iter().find(|domain| domain["name"] == STALWART_DOMAIN))
            .and_then(|domain| domain["id"].as_str())
            .unwrap_or_else(|| panic!("Stalwart's bootstrap made no {STALWART_DOMAIN}: {domains}"))
            .to_string();
        let made = server
            .stalwart_admin(
                password,
                json!([["x:Account/set", {"create": {"u": {
                    "@type": "User", "name": "me", "domainId": domain,
                    "credentials": {"0": {"@type": "Password", "secret": password}}
                }}}, "0"]]),
            )
            .await;
        assert!(
            made["methodResponses"][0][1]["created"]["u"].is_object(),
            "Stalwart did not make the test user: {made}"
        );
        if !answers(&server.session_url, Some((&server.user, password))).await {
            return server
                .container
                .not_ready(REQUIRE_JMAP, "Stalwart's test user", READY_WITHIN)
                .await;
        }
        Some(server)
    }

    /// Calls on Stalwart's management capability as the recovery
    /// administrator.
    async fn stalwart_admin(&self, password: &str, calls: Value) -> Value {
        self.call_as(
            STALWART_ADMIN,
            password,
            &["urn:ietf:params:jmap:core", "urn:stalwart:jmap"],
            calls,
        )
        .await
    }

    async fn james(password: &str) -> Option<JmapServer> {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let keystore = james_keystore(dir.path())?;
        let port = free_port();
        let base = format!("http://localhost:{port}");
        let config = dir.path().join("jmap.properties");
        std::fs::write(
            &config,
            format!(
                "{}url.prefix={base}\n",
                include_str!("../jmap/james-jmap.properties")
            ),
        )
        .expect("write James's JMAP settings");
        let options = vec![
            "-p".to_string(),
            format!("127.0.0.1:{port}:80"),
            "-e".into(),
            "DOMAIN=james.test".into(),
        ];
        let files = [
            (config, "/root/conf/jmap.properties"),
            (keystore, "/root/conf/keystore"),
        ];
        let container = Container::start(REQUIRE_JMAP, &options, JAMES_IMAGE, &[], &files).await?;
        if !answers(&format!("{base}/.well-known/jmap"), None).await {
            return container
                .not_ready(REQUIRE_JMAP, "Apache James", READY_WITHIN)
                .await;
        }
        let added = container
            .exec(&["james-cli", "AddUser", "me@james.test", password], None)
            .await;
        assert!(
            added.status.success(),
            "james-cli AddUser failed: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        Some(JmapServer {
            container,
            http: reqwest::Client::new(),
            kind: JmapKind::James,
            session_url: format!("{base}/jmap/session"),
            user: "me@james.test".into(),
            address: "me@james.test".into(),
        })
    }

    async fn call_as(&self, user: &str, password: &str, using: &[&str], calls: Value) -> Value {
        let http = &self.http;
        let session: Value = http
            .get(&self.session_url)
            .basic_auth(user, Some(password))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .expect("the session")
            .json()
            .await
            .expect("the session's JSON");
        let api = endpoint(&self.session_url, session["apiUrl"].as_str().expect("an apiUrl"), "");
        http.post(api)
            .basic_auth(user, Some(password))
            .json(&json!({ "using": using, "methodCalls": calls }))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .expect("the call")
            .json()
            .await
            .expect("the answer's JSON")
    }

    /// Method calls as this server's own user, through a client the tests
    /// trust rather than the one under test.
    pub async fn call(&self, password: &str, using: &[&str], calls: Value) -> Value {
        self.call_as(&self.user, password, using, calls).await
    }

    /// The session as this server's own user sees it.
    pub async fn session(&self, password: &str) -> Value {
        self.http
            .get(&self.session_url)
            .basic_auth(&self.user, Some(password))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .expect("the session")
            .json()
            .await
            .expect("the session's JSON")
    }

    /// The user's primary mail account id.
    pub async fn mail_account(&self, password: &str) -> String {
        let session = self.session(password).await;
        session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
            .as_str()
            .expect("a mail account")
            .to_string()
    }

    /// Hands `raw` to the server's own delivery, so the account's active
    /// Sieve script runs on it, as it would on mail from outside. An import
    /// skips Sieve. Cyrus takes it over LMTP and Stalwart over SMTP, both
    /// spoken from inside the container by the Perl each image carries,
    /// so no further port is published. Stalwart queues it and delivers a
    /// moment later.
    pub async fn deliver_through_rules(&self, raw: &str) {
        let (port, greeting) = match self.kind {
            JmapKind::Cyrus => ("8024", "LHLO"),
            JmapKind::Stalwart => ("25", "EHLO"),
            JmapKind::James => panic!("James runs no Sieve over JMAP"),
        };
        let script = DELIVER_SCRIPT.replace("{port}", port).replace("{greeting}", greeting);
        let mut child = Command::new("docker")
            .args(["exec", "-i", self.id(), "perl", "-e", &script, &self.address])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("docker runs");
        {
            use std::io::Write;
            child.stdin.take().expect("a stdin").write_all(raw.as_bytes()).expect("the message goes in");
        }
        let done = child.wait_with_output().expect("docker answers");
        assert!(done.status.success(), "{:?} refused the delivery: {}", self.kind, String::from_utf8_lossy(&done.stderr));
    }

    /// Imports `raw` into the Inbox as another client would, and answers
    /// the new Email's id.
    pub async fn deliver(&self, password: &str, raw: &[u8]) -> String {
        let http = &self.http;
        let session = self.session(password).await;
        let account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
            .as_str()
            .expect("a mail account")
            .to_string();
        let upload = endpoint(&self.session_url, session["uploadUrl"].as_str().expect("an uploadUrl"), &account);
        let uploaded: Value = http
            .post(upload)
            .basic_auth(&self.user, Some(password))
            .header("Content-Type", "message/rfc822")
            .body(raw.to_vec())
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .expect("the upload")
            .json()
            .await
            .expect("the upload's JSON");
        let blob = uploaded["blobId"].as_str().expect("a blobId").to_string();
        let using = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];
        // Mailbox/get rather than a query by role: James makes a new user's
        // mailboxes on the first Mailbox/get, and a query before it finds
        // none.
        let mailboxes = self
            .call(
                password,
                &using,
                json!([["Mailbox/get", {"accountId": account, "ids": null, "properties": ["role"]}, "0"]]),
            )
            .await;
        let inbox = mailboxes["methodResponses"][0][1]["list"]
            .as_array()
            .and_then(|list| list.iter().find(|mailbox| mailbox["role"] == "inbox"))
            .and_then(|mailbox| mailbox["id"].as_str())
            .unwrap_or_else(|| panic!("no Inbox: {mailboxes}"))
            .to_string();
        let imported = self
            .call(
                password,
                &using,
                json!([["Email/import", {"accountId": account, "emails": {"m": {
                    "blobId": blob, "mailboxIds": {inbox: true}, "keywords": {}
                }}}, "0"]]),
            )
            .await;
        imported["methodResponses"][0][1]["created"]["m"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("the import made nothing: {imported}"))
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::endpoint;

    #[test]
    fn a_relative_upload_url_resolves_against_the_session() {
        let url = endpoint("http://localhost:4000/jmap/session", "/jmap/upload/{accountId}/", "a1");
        assert_eq!(url.as_str(), "http://localhost:4000/jmap/upload/a1/");
    }

    #[test]
    fn an_absolute_upload_url_stays_as_the_session_wrote_it() {
        let url = endpoint("http://localhost:4000/jmap", "http://localhost:4000/jmap/upload/{accountId}/", "a1");
        assert_eq!(url.as_str(), "http://localhost:4000/jmap/upload/a1/");
    }
}
