//! Blobs (RFC 8620 section 6): a raw message or one part of it down, a
//! message to import up. Every transfer holds one of the account's request
//! slots from start to end and stops at a cap. A download to a file writes
//! beside the target and renames it once whole, so a failure leaves no
//! half file. A filled download URL holds the account id, which can be an
//! address, so no error here repeats a URL.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderValue};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::error::decode;
use crate::http::{failure, network, read_capped};
use crate::{JmapClient, JmapError};

/// The most one blob may take, the same as a gpg run and a POP3 message.
pub const MOST_BLOB_BYTES: usize = 64 << 20;

/// How long a blob transfer may take. A 64 MiB file on a slow line needs
/// minutes, which a call's timeout would cut short.
const BLOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// What an upload's answer may take.
const UPLOAD_ANSWER_LIMIT: usize = 64 << 10;

/// A blob the server now holds (RFC 8620 section 6.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Uploaded {
    pub account_id: String,
    pub blob_id: String,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub size: u64,
}

/// `template` with each `{name}` replaced by its value, percent-encoded
/// so a value cannot add a path segment or a query (RFC 6570 level 1).
pub fn expand(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = out.replace(&format!("{{{name}}}"), &encode(value));
    }
    out
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(char::from(byte)),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// What a local file error says, without the path.
fn io(err: std::io::Error) -> JmapError {
    JmapError::LocalFile(err.kind().to_string())
}

/// A partial download, deleted when dropped unless `keep` is set.
struct PartFile {
    path: PathBuf,
    keep: bool,
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.keep {
            // A blocking unlink of one small file; there is nothing to
            // report if it is already gone.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl JmapClient {
    /// The open answer for a blob. The caller holds a permit.
    async fn blob_response(&self, url: &str) -> Result<reqwest::Response, JmapError> {
        // `endpoint` repeats a refused URL, and this one holds the account
        // id, so the refusal names the kind of URL instead.
        let url = self.endpoint(url).map_err(|_| JmapError::BadUrl("the download URL".into()))?;
        let response = self.authorized(reqwest::Method::GET, url).await?.timeout(BLOB_TIMEOUT).send().await.map_err(network)?;
        match response.status().is_success() {
            true => Ok(response),
            false => Err(failure(response).await),
        }
    }

    async fn download_url(&self, account_id: &str, blob_id: &str, name: &str, mime: &str) -> Result<String, JmapError> {
        let session = self.session().await?;
        let values = [("accountId", account_id), ("blobId", blob_id), ("name", name), ("type", mime)];
        Ok(expand(&session.download_url, &values))
    }

    /// The blob's bytes, refused once they pass `limit`.
    pub async fn download(&self, account_id: &str, blob_id: &str, name: &str, mime: &str, limit: usize) -> Result<Vec<u8>, JmapError> {
        // The session first: reading it takes a permit of its own.
        let url = self.download_url(account_id, blob_id, name, mime).await?;
        let _permit = self.permit().await;
        let response = self.blob_response(&url).await?;
        read_capped(response, limit).await
    }

    /// Writes the blob to `to`, a chunk at a time, and answers its size.
    /// Nothing is left at `to` or beside it when the download fails.
    pub async fn download_to(
        &self,
        account_id: &str,
        blob_id: &str,
        name: &str,
        mime: &str,
        to: &Path,
        limit: usize,
    ) -> Result<u64, JmapError> {
        let url = self.download_url(account_id, blob_id, name, mime).await?;
        let _permit = self.permit().await;
        let mut response = self.blob_response(&url).await?;
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err(JmapError::AnswerTooLarge { limit });
        }
        let mut partial = OsString::from(to);
        partial.push(".part");
        // Removes the partial file on any way out that does not rename it,
        // including the caller dropping this future.
        let mut guard = PartFile { path: partial.into(), keep: false };
        let mut file = tokio::fs::File::create(&guard.path).await.map_err(io)?;
        let mut written: u64 = 0;
        while let Some(chunk) = response.chunk().await.map_err(network)? {
            written += chunk.len() as u64;
            if written > limit as u64 {
                return Err(JmapError::AnswerTooLarge { limit });
            }
            file.write_all(&chunk).await.map_err(io)?;
        }
        file.flush().await.map_err(io)?;
        drop(file);
        tokio::fs::rename(&guard.path, to).await.map_err(io)?;
        guard.keep = true;
        Ok(written)
    }

    /// Uploads `bytes` as a blob of `mime` to `account_id`, refused before
    /// it goes when it passes the session's `maxSizeUpload`.
    pub async fn upload(&self, account_id: &str, mime: &str, bytes: Vec<u8>) -> Result<Uploaded, JmapError> {
        let session = self.session().await?;
        if bytes.len() as u64 > session.limits().max_size_upload {
            return Err(JmapError::RequestTooLarge);
        }
        let url = expand(&session.upload_url, &[("accountId", account_id)]);
        let url = self.endpoint(&url).map_err(|_| JmapError::BadUrl("the upload URL".into()))?;
        let mime = HeaderValue::from_str(mime).map_err(|_| JmapError::Problem {
            status: 0,
            kind: "mime".into(),
            detail: "the type is not a valid header value".into(),
        })?;
        let _permit = self.permit().await;
        let response = self
            .authorized(reqwest::Method::POST, url)
            .await?
            .header(CONTENT_TYPE, mime)
            .timeout(BLOB_TIMEOUT)
            .body(bytes)
            .send()
            .await
            .map_err(network)?;
        if !response.status().is_success() {
            return Err(failure(response).await);
        }
        let body = read_capped(response, UPLOAD_ANSWER_LIMIT).await?;
        serde_json::from_slice(&body).map_err(|e| decode("the upload answer", &e))
    }
}
