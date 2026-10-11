//! Push against wiremock: no URL, a change, and a stream gone quiet.

mod common;

use std::time::Duration;

use mailrs_jmap::{JmapError, Pushed};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{client, session_on};

#[tokio::test]
async fn a_session_without_event_source_says_so() {
    let server = MockServer::start().await;
    let mut session = session_on(&server.uri(), json!({}));
    session.as_object_mut().unwrap().remove("eventSourceUrl");
    Mock::given(method("GET")).and(path("/jmap/session")).respond_with(ResponseTemplate::new(200).set_body_json(session)).mount(&server).await;
    assert_eq!(client(&server).push(Duration::from_secs(1)).await.unwrap(), Pushed::NoPush);
}

#[tokio::test]
async fn a_change_to_the_mail_account_wakes_the_watch() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({}))))
        .mount(&server).await;
    let body = "event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"OTHER\":{\"Email\":\"x\"}}}\n\n\
                event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"A1\":{\"Email\":\"e2\"}}}\n\n";
    Mock::given(method("GET"))
        .and(path("/events/"))
        .and(query_param("types", "Email,Mailbox,EmailDelivery"))
        .and(query_param("closeafter", "no"))
        .and(query_param("ping", "60"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "text/event-stream").set_body_string(body))
        .mount(&server)
        .await;
    assert_eq!(client(&server).push(Duration::from_secs(5)).await.unwrap(), Pushed::Changed);
}

#[tokio::test]
async fn a_stream_that_closes_ends_the_watch() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({}))))
        .mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/events/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/event-stream")
                .set_body_string(": hello\n\n")
                .set_delay(Duration::from_millis(10)),
        )
        .mount(&server)
        .await;
    let refused = client(&server).push(Duration::from_millis(300)).await.unwrap_err();
    assert!(matches!(&refused, JmapError::Network(said) if said.contains("ended")), "{refused:?}");
}

#[tokio::test]
async fn a_line_without_an_end_past_the_cap_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({}))))
        .mount(&server).await;
    let body = "x".repeat(mailrs_jmap::push::MOST_EVENT_BYTES + 10);
    Mock::given(method("GET")).and(path("/events/"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "text/event-stream").set_body_string(body))
        .mount(&server).await;
    let refused = client(&server).push(Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(refused, JmapError::AnswerTooLarge { .. }), "{refused:?}");
}

#[tokio::test]
async fn a_failed_open_answers_the_problem_and_leaks_no_address() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_on(&server.uri(), json!({}))))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/events/")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    let refused = client(&server).push(Duration::from_secs(5)).await.unwrap_err();
    assert!(!refused.to_string().contains("127.0.0.1"), "{refused}");
}

/// Answers the event stream's head and one comment, then holds the socket
/// open and says nothing more. Answers its address.
async fn silent_server() -> String {
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
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                let hello = ": hello\n\n";
                let first = format!("{head}{:x}\r\n{hello}\r\n", hello.len());
                if stream.write_all(first.as_bytes()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn a_silent_stream_ends_the_watch_after_the_idle_limit() {
    let events = silent_server().await;
    let server = MockServer::start().await;
    let mut session = session_on(&server.uri(), json!({}));
    session["eventSourceUrl"] = json!(format!("{events}/events/"));
    Mock::given(method("GET")).and(path("/jmap/session")).respond_with(ResponseTemplate::new(200).set_body_json(session)).mount(&server).await;
    let started = std::time::Instant::now();
    let refused = client(&server).push(Duration::from_millis(300)).await.unwrap_err();
    assert!(matches!(&refused, JmapError::Network(said) if said.contains("quiet")), "{refused:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}
