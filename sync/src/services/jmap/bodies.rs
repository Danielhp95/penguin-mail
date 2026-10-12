//! Raw messages, and the structure and parts of one too large to fetch
//! whole. An Email's `blobId` is its raw message: one capped download
//! feeds the `mailrs_mime` pipeline unchanged. A large message's parts
//! keep IMAP's section numbers, so a file opened later is found the same
//! way for every provider, and its blob comes alone.

use mailrs_jmap::{BODY, BodyPart, MOST_BLOB_BYTES};
use mailrs_mime::{MAX_DEPTH, content_id, numbered as section};
use mailrs_mime::{Part, Parts};

use super::{Jmap, JmapApi, backend};
use crate::services::imap::bodies::text_paths;
use crate::BackendError;
use crate::services::RawMessage;

/// `root` as a part tree numbered the way IMAP numbers sections: a
/// multipart root has an empty path, a single part is `1`.
pub(crate) fn numbered(root: &BodyPart) -> Part {
    let path = match root.mime_type.to_ascii_lowercase().starts_with("multipart/") {
        true => String::new(),
        false => "1".to_string(),
    };
    number(root, path, 0)
}

fn number(part: &BodyPart, path: String, depth: usize) -> Part {
    let children = match depth < MAX_DEPTH {
        true => part
            .sub_parts
            .iter()
            .flatten()
            .enumerate()
            .map(|(i, child)| number(child, section(&path, i), depth + 1))
            .collect(),
        false => Vec::new(),
    };
    Part {
        path,
        mime_type: part.mime_type.to_ascii_lowercase(),
        charset: part.charset.clone(),
        filename: part.name.clone(),
        content_id: part.cid.as_deref().map(content_id),
        attachment: part.disposition.as_deref().is_some_and(|d| d.eq_ignore_ascii_case("attachment")),
        size: i64::try_from(part.size).unwrap_or(i64::MAX),
        children,
        ..Part::default()
    }
}

/// Each leaf's section number with its blob id.
fn handles_of(part: &Part, source: &BodyPart, out: &mut Vec<(String, String)>) {
    if part.children.is_empty() {
        if let Some(blob) = &source.blob_id {
            out.push((part.path.clone(), blob.clone()));
        }
        return;
    }
    for (child, source) in part.children.iter().zip(source.sub_parts.iter().flatten()) {
        handles_of(child, source, out);
    }
}

/// The most bytes one `raws` call holds, all its messages together. A
/// message alone may take `MOST_BLOB_BYTES`; an export asks for a batch
/// whose stored sizes add up to far less, so only sizes that lied reach
/// this.
#[cfg(not(test))]
pub(crate) const RAWS_BUDGET: usize = 2 * MOST_BLOB_BYTES;
/// Small in tests, so a few short messages pass it.
#[cfg(test)]
pub(crate) const RAWS_BUDGET: usize = 1000;

impl<J: JmapApi> Jmap<J> {
    /// The messages `ids` name, whole and in order. Each download may take
    /// what the call's budget has left, and one past it is refused as too
    /// large.
    pub(super) async fn raws(&self, ids: &[String]) -> Result<Vec<RawMessage>, BackendError> {
        let got = self.api().emails(ids, &["id", "blobId"]).await.map_err(backend)?;
        let mut out = Vec::with_capacity(got.list.len());
        let mut left = RAWS_BUDGET;
        for id in ids {
            let Some(email) = got.list.iter().find(|e| &e.id == id) else {
                return Err(BackendError::NotFound);
            };
            let bytes = self.api().download(&email.blob_id, MOST_BLOB_BYTES.min(left)).await.map_err(backend)?;
            left = left.saturating_sub(bytes.len());
            out.push(RawMessage { id: id.clone(), bytes });
        }
        Ok(out)
    }

    pub(super) async fn structure(&self, id: &str) -> Result<Parts, BackendError> {
        let got = self.api().emails(&[id.to_string()], BODY).await.map_err(backend)?;
        let email = got.list.into_iter().next().ok_or(BackendError::NotFound)?;
        let source = email.body_structure.ok_or(BackendError::NotFound)?;
        let mut handles = Vec::new();
        let headers = email
            .headers
            .unwrap_or_default()
            .into_iter()
            .map(|h| (h.name, h.value.trim().to_string()))
            .collect();
        let mut parts = Parts { headers, root: numbered(&source), incomplete: false };
        handles_of(&parts.root, &source, &mut handles);
        // The text parts of one message share one cap, so many of them
        // cannot add up to more than a whole message may be.
        let mut left = MOST_BLOB_BYTES;
        for path in text_paths(&parts) {
            let Some((_, blob)) = handles.iter().find(|(p, _)| *p == path) else {
                parts.incomplete = true;
                continue;
            };
            match self.api().download(blob, left).await {
                Ok(bytes) => {
                    left = left.saturating_sub(bytes.len());
                    parts.set_data(&path, bytes);
                }
                Err(_) => parts.incomplete = true,
            }
        }
        self.handles().remember(id, handles);
        Ok(parts)
    }

    pub(super) async fn part(&self, id: &str, path: &str) -> Result<Vec<u8>, BackendError> {
        let known = self.handles().get(id, path);
        let blob = match known {
            Some(blob) => blob,
            None => {
                self.structure(id).await?;
                self.handles().get(id, path).ok_or(BackendError::NotFound)?
            }
        };
        self.api().download(&blob, MOST_BLOB_BYTES).await.map_err(backend)
    }
}
