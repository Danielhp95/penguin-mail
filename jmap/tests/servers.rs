//! The client against Cyrus, Stalwart and Apache James in Docker: the
//! session over Basic, the check, a call, and a blob up and down. One test,
//! as every Docker file here has.

use mailrs_jmap::{Auth, JmapClient, MAIL, MOST_BLOB_BYTES, Request, check};
use mailrs_testmail::{JmapKind, JmapServer};
use serde_json::json;

#[test]
fn the_client_signs_in_and_moves_a_blob_on_every_server() {
    if mailrs_testmail::jmap_docker().is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    runtime.block_on(async {
        let password = mailrs_testmail::password();
        for kind in JmapKind::ALL {
            eprintln!("{kind:?}");
            let Some(server) = JmapServer::start(kind, &password).await else {
                return;
            };
            let client = JmapClient::new(&server.session_url, Auth::Basic { user: server.user.clone(), password: password.clone() })
                .expect("a client for the server's session");
            let checked = check(&client).await.expect("the server takes the password");
            assert!(checked.session.offers(MAIL), "{kind:?} offers mail");
            assert!(checked.offered.submission, "{kind:?} offers sending");
            // Cyrus signs in as `user1` and its identity has an empty email,
            // so the check finds no address.
            let address = match kind {
                JmapKind::Cyrus => None,
                JmapKind::Stalwart | JmapKind::James => Some(server.address.as_str()),
            };
            assert_eq!(checked.address.as_deref(), address, "{kind:?}");

            let account = checked.session.account_for(MAIL).expect("a mail account").to_string();
            let mut request = Request::new();
            let id = request.call(MAIL, "Mailbox/get", json!({ "accountId": account, "ids": null, "properties": ["role"] }));
            let mut answer = client.send(&request).await.expect("Mailbox/get");
            let listed: serde_json::Value = answer.take(&id, "Mailbox/get").expect("the mailboxes");
            assert!(listed["list"].as_array().is_some_and(|l| l.iter().any(|m| m["role"] == "inbox")), "{kind:?}: {listed}");

            let raw = b"From: a@example.test\r\nTo: b@example.test\r\nSubject: blob\r\n\r\nbody\r\n".to_vec();
            let uploaded = client.upload(&account, "message/rfc822", raw.clone()).await.expect("an upload");
            let back = client
                .download(&account, &uploaded.blob_id, "m.eml", "message/rfc822", MOST_BLOB_BYTES)
                .await
                .expect("the blob back");
            assert_eq!(back, raw, "{kind:?} returns the bytes it took");
        }
    });
}
