//! Each JMAP server starts, takes its user's password, and keeps a message
//! another client imports. One test, as every Docker file here has.

use mailrs_testmail::{JmapKind, JmapServer};
use serde_json::json;

#[test]
fn each_jmap_server_starts_and_takes_its_user() {
    if mailrs_testmail::jmap_docker().is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let password = mailrs_testmail::password();
        for kind in JmapKind::ALL {
            let Some(server) = JmapServer::start(kind, &password).await else {
                continue;
            };
            let id = server
                .deliver(&password, b"From: a@example.test\r\nTo: me@example.test\r\nSubject: hello\r\nMessage-ID: <j1@example.test>\r\n\r\nhi\r\n")
                .await;
            let account = server.mail_account(&password).await;
            let got = server
                .call(
                    &password,
                    &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
                    json!([["Email/get", {"accountId": account, "ids": [id], "properties": ["subject"]}, "0"]]),
                )
                .await;
            assert_eq!(got["methodResponses"][0][1]["list"][0]["subject"], "hello", "{kind:?}: {got}");
        }
    });
}
