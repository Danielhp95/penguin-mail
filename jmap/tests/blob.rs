//! Blob downloads under a cap, to memory and to a file, on the host the
//! session names; uploads under the session's own limit.

mod common;

use mailrs_jmap::{JmapError, MOST_BLOB_BYTES};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{client, session_on};

async fn session_with_downloads_on(server: &MockServer, downloads: &str) {
    let mut session = session_on(&server.uri(), json!({}));
    session["downloadUrl"] = json!(format!("{downloads}/download/{{accountId}}/{{blobId}}/{{name}}?type={{type}}"));
    Mock::given(method("GET"))
        .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_blob_downloads_from_the_host_the_session_names() {
    let (server, content) = (MockServer::start().await, MockServer::start().await);
    session_with_downloads_on(&server, &content.uri()).await;
    Mock::given(method("GET"))
        .and(path("/download/A1/B%201/message.eml"))
        .and(header("Authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"From: a@b\r\n\r\nhi".to_vec()))
        .mount(&content)
        .await;
    let bytes = client(&server).download("A1", "B 1", "message.eml", "message/rfc822", MOST_BLOB_BYTES).await.unwrap();
    assert_eq!(bytes, b"From: a@b\r\n\r\nhi");
}

#[tokio::test]
async fn a_blob_with_a_length_past_the_cap_is_refused_before_it_is_read() {
    let server = MockServer::start().await;
    session_with_downloads_on(&server, &server.uri()).await;
    // wiremock always sets Content-Length, so this reaches the early
    // refusal. The next tests cover a body with no length.
    Mock::given(method("GET"))
        .and(path("/download/A1/B1/x"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'a'; 2048]))
        .mount(&server)
        .await;
    let refused = client(&server).download("A1", "B1", "x", "application/octet-stream", 1024).await;
    assert_eq!(refused, Err(JmapError::AnswerTooLarge { limit: 1024 }));
}

#[tokio::test]
async fn a_download_to_a_file_streams_and_leaves_nothing_half_written() {
    let server = MockServer::start().await;
    session_with_downloads_on(&server, &server.uri()).await;
    for blob in ["B1/a.pdf", "B2/b.pdf"] {
        Mock::given(method("GET"))
            .and(path(format!("/download/A1/{blob}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 4096]))
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let client = client(&server);
    let kept = dir.path().join("a.pdf");
    assert_eq!(client.download_to("A1", "B1", "a.pdf", "application/pdf", &kept, 8192).await.unwrap(), 4096);
    assert_eq!(std::fs::read(&kept).unwrap().len(), 4096);
    let refused = dir.path().join("b.pdf");
    assert!(client.download_to("A1", "B2", "b.pdf", "application/pdf", &refused, 1024).await.is_err());
    assert!(!refused.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no partial file is left behind");
}

#[tokio::test]
async fn an_upload_names_the_account_and_answers_the_blob() {
    let server = MockServer::start().await;
    session_with_downloads_on(&server, &server.uri()).await;
    Mock::given(method("POST"))
        .and(path("/upload/A1/"))
        .and(header("Content-Type", "message/rfc822"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "accountId": "A1", "blobId": "G1", "type": "message/rfc822", "size": 5
        })))
        .mount(&server)
        .await;
    let uploaded = client(&server).upload("A1", "message/rfc822", b"hello".to_vec()).await.unwrap();
    assert_eq!(uploaded.blob_id, "G1");
    assert_eq!(uploaded.size, 5);
}

#[tokio::test]
async fn an_upload_past_the_session_limit_never_goes() {
    let server = MockServer::start().await;
    session_with_downloads_on(&server, &server.uri()).await;
    let refused = client(&server).upload("A1", "message/rfc822", vec![0; 1_000_001]).await;
    assert_eq!(refused.unwrap_err(), JmapError::RequestTooLarge);
    assert!(server.received_requests().await.unwrap().iter().all(|r| r.method.as_str() != "POST"));
}

#[test]
fn template_values_are_percent_encoded() {
    let url = mailrs_jmap::blob::expand(
        "https://x/d/{accountId}/{blobId}/{name}?type={type}",
        &[("accountId", "A1"), ("blobId", "B/1"), ("name", "a b.eml"), ("type", "message/rfc822")],
    );
    assert_eq!(url, "https://x/d/A1/B%2F1/a%20b.eml?type=message%2Frfc822");
}

#[tokio::test]
async fn a_failed_download_does_not_repeat_the_account_in_its_error() {
    let server = MockServer::start().await;
    // Nothing listens on port 1, so the transfer fails at connect.
    session_with_downloads_on(&server, "http://127.0.0.1:1").await;
    let account = "me@example.org";
    let err = client(&server).download(account, "B1", "x", "text/plain", 1024).await.unwrap_err();
    assert!(matches!(err, JmapError::Network(_)), "{err:?}");
    assert!(!err.to_string().contains(account), "{err}");
}

#[tokio::test]
async fn a_download_holds_a_permit_for_its_whole_transfer() {
    let server = MockServer::start().await;
    session_with_downloads_on(&server, &server.uri()).await;
    Mock::given(method("GET"))
        .and(path("/download/A1/B1/slow"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".to_vec()).set_delay(std::time::Duration::from_millis(300)))
        .mount(&server)
        .await;
    // The session allows two requests at once, so a third waits.
    let client = client(&server);
    let started = std::time::Instant::now();
    let one = || client.download("A1", "B1", "slow", "text/plain", 1024);
    let (a, b, c) = tokio::join!(one(), one(), one());
    for result in [a, b, c] {
        result.unwrap();
    }
    assert!(started.elapsed() >= std::time::Duration::from_millis(600), "{:?}", started.elapsed());
}

/// A server that answers every request with a chunked body of `chunks`
/// chunks of 1 KiB, `pause` apart, and no Content-Length. Answers its address.
async fn chunked_server(chunks: usize, pause: std::time::Duration) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 1024];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                if stream.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                for _ in 0..chunks {
                    let chunk = format!("400\r\n{}\r\n", "a".repeat(1024));
                    if stream.write_all(chunk.as_bytes()).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(pause).await;
                }
                let _ = stream.write_all(b"0\r\n\r\n").await;
            });
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn a_chunked_blob_without_a_length_stops_at_the_cap() {
    let server = MockServer::start().await;
    session_with_downloads_on(&server, &chunked_server(4, std::time::Duration::ZERO).await).await;
    let refused = client(&server).download("A1", "B1", "x", "text/plain", 1024).await;
    assert_eq!(refused, Err(JmapError::AnswerTooLarge { limit: 1024 }));
}

#[tokio::test]
async fn a_chunked_download_to_a_file_stops_at_the_cap_and_leaves_no_file() {
    let server = MockServer::start().await;
    session_with_downloads_on(&server, &chunked_server(4, std::time::Duration::ZERO).await).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("x.bin");
    let refused = client(&server).download_to("A1", "B1", "x", "text/plain", &target, 1024).await;
    assert_eq!(refused, Err(JmapError::AnswerTooLarge { limit: 1024 }));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "neither the target nor a .part file remains");
}

#[tokio::test]
async fn a_cancelled_download_to_a_file_leaves_no_part_file() {
    let server = MockServer::start().await;
    let slow = chunked_server(10, std::time::Duration::from_millis(200)).await;
    session_with_downloads_on(&server, &slow).await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("x.bin");
    let client = client(&server);
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(700),
        client.download_to("A1", "B1", "x", "text/plain", &target, 1 << 20),
    )
    .await;
    assert!(cancelled.is_err(), "the transfer should still be running");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
