//! Connection-owned result snapshots and bounded wire framing.
//!
//! Large reads do not re-resolve a result for each page. One authenticated
//! request freezes the already-redacted document until this connection closes.
//! There is no global transfer cache, resumable credential or implicit retry.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{
    Document, MAX_COMPLETE_BYTES, MAX_DOCUMENT_BYTES, WireResponse, current_identity,
    enforce_document_limit, ipc_error,
};
use crate::error::Result;

const PAGE_BYTES: usize = 16 * 1024;
const METADATA_BYTES: usize = 32 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Header {
    content_type: String,
    metadata: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Page {
    reference: String,
    offset: usize,
    total_bytes: usize,
    fingerprint: String,
    content: String,
    done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    header: Option<Header>,
}

/// Shared byte counting includes escaping and metadata, but never creates a
/// second encoded copy of a large source just to decide whether it fits.
pub(super) fn encoded_size(value: &impl Serialize, limit: usize) -> Option<usize> {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(io::Error::other("result byte limit"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter { used: 0, limit };
    serde_json::to_writer(&mut count, value).ok()?;
    Some(count.used)
}

fn fingerprint(document: &Document) -> Result<String> {
    struct Sink(Sha256);
    impl io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut sink = Sink(Sha256::new());
    serde_json::to_writer(&mut sink, document)?;
    Ok(crate::package_manager::hex_encode(&sink.0.finalize()))
}

fn validate_metadata(content_type: &str, metadata: &Value) -> Result<()> {
    if content_type.len() > 128
        || !metadata.is_object()
        || encoded_size(metadata, METADATA_BYTES).is_none()
    {
        return Err(ipc_error(
            "RESULT_LIMIT",
            "agent result metadata exceeds its bounds",
        ));
    }
    Ok(())
}

struct Pages {
    document: Option<Document>,
    fingerprint: String,
    cursor: usize,
    complete: bool,
}

impl Pages {
    fn new(document: Document) -> Result<Self> {
        super::reference_valid(&document.reference)?;
        enforce_document_limit(&document)?;
        validate_metadata(&document.content_type, &document.metadata)?;
        let complete = encoded_size(&document, MAX_COMPLETE_BYTES).is_some();
        let fingerprint = if complete {
            String::new()
        } else {
            fingerprint(&document)?
        };
        Ok(Self {
            document: Some(document),
            fingerprint,
            cursor: 0,
            complete,
        })
    }

    fn next(&mut self) -> Option<WireResponse> {
        if self.complete {
            return self.document.take().map(WireResponse::Ok);
        }
        let document = self.document.as_ref()?;
        let total = document.content.len();
        let start = self.cursor;
        let mut end = start.saturating_add(PAGE_BYTES).min(total);
        while !document.content.is_char_boundary(end) {
            end -= 1;
        }
        let page = Page {
            reference: document.reference.clone(),
            offset: start,
            total_bytes: total,
            fingerprint: self.fingerprint.clone(),
            content: document.content[start..end].to_string(),
            done: end == total,
            header: (start == 0).then(|| Header {
                content_type: document.content_type.clone(),
                metadata: document.metadata.clone(),
            }),
        };
        self.cursor = end;
        if page.done {
            self.document = None;
        }
        Some(WireResponse::Page(page))
    }
}

/// One accepted snapshot belongs to one authenticated connection and reader.
/// Dropping the connection releases it immediately, even mid-frame.
pub struct Transfer {
    pages: Pages,
    sender: String,
    identity: (PathBuf, u32),
}

impl std::fmt::Debug for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentResultTransfer")
            .field("pending", &self.pages.document.is_some())
            .finish_non_exhaustive()
    }
}

impl Transfer {
    pub(crate) fn new(document: Document, sender: &str, identity: (PathBuf, u32)) -> Result<Self> {
        Ok(Self {
            pages: Pages::new(document)?,
            sender: sender.to_string(),
            identity,
        })
    }

    pub(crate) fn check_reader(&self) -> Result<()> {
        self.check_identity(&current_identity(&self.sender)?)
    }

    fn check_identity(&self, observed: &(PathBuf, u32)) -> Result<()> {
        if *observed != self.identity {
            return Err(ipc_error(
                "REVOKED",
                "result reader identity changed during transfer",
            ));
        }
        Ok(())
    }

    pub(crate) fn next_response(&mut self) -> Result<Option<WireResponse>> {
        if self.pages.document.is_none() {
            return Ok(None);
        }
        self.next_with_identity(current_identity(&self.sender))
    }

    fn next_with_identity(
        &mut self,
        observed: Result<(PathBuf, u32)>,
    ) -> Result<Option<WireResponse>> {
        self.check_identity(&observed?)?;
        Ok(self.pages.next())
    }
}

struct Partial {
    document: Document,
    fingerprint: String,
    total: usize,
}

/// A caller receives no document until every page and the final integrity
/// check succeed. Errors, missing frames and cancellation discard partials.
pub(super) struct Collector<'a> {
    reference: &'a str,
    partial: Option<Partial>,
    finished: bool,
}

impl<'a> Collector<'a> {
    pub(super) const fn new(reference: &'a str) -> Self {
        Self {
            reference,
            partial: None,
            finished: false,
        }
    }

    pub(super) fn push(&mut self, response: WireResponse) -> Result<Option<Document>> {
        if self.finished {
            return Err(ipc_error(
                "PROTOCOL",
                "data followed the completed agent result",
            ));
        }
        let outcome = self.push_inner(response);
        if outcome.is_err() {
            self.finished = true;
            self.partial = None;
        }
        outcome
    }

    fn push_inner(&mut self, response: WireResponse) -> Result<Option<Document>> {
        match response {
            WireResponse::Error(error) => Err(crate::error::Error::tool("read", error)),
            WireResponse::Ok(document) => {
                if self.partial.is_some() || document.reference != self.reference {
                    return Err(ipc_error(
                        "PROTOCOL",
                        "parent returned a mismatched result document",
                    ));
                }
                validate_metadata(&document.content_type, &document.metadata)?;
                if encoded_size(&document, MAX_COMPLETE_BYTES).is_none() {
                    return Err(ipc_error(
                        "RESULT_LIMIT",
                        "complete result reply exceeds 128 KiB; paging is required",
                    ));
                }
                self.finished = true;
                Ok(Some(document))
            }
            WireResponse::Page(page) => self.push_page(page),
        }
    }

    fn push_page(&mut self, page: Page) -> Result<Option<Document>> {
        if page.reference != self.reference
            || page.total_bytes > MAX_DOCUMENT_BYTES
            || page.total_bytes == 0
            || page.content.is_empty()
            || page.content.len() > PAGE_BYTES
            || page.fingerprint.len() != 64
            || !page.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ipc_error(
                "PROTOCOL",
                "invalid agent result page bounds or identity",
            ));
        }
        if self.partial.is_none() {
            let header = page
                .header
                .ok_or_else(|| ipc_error("PROTOCOL", "first result page has no header"))?;
            if page.offset != 0 {
                return Err(ipc_error(
                    "PROTOCOL",
                    "first result page does not begin at zero",
                ));
            }
            validate_metadata(&header.content_type, &header.metadata)?;
            self.partial = Some(Partial {
                document: Document {
                    reference: page.reference,
                    content: String::new(),
                    content_type: header.content_type,
                    metadata: header.metadata,
                },
                fingerprint: page.fingerprint.clone(),
                total: page.total_bytes,
            });
        } else if page.header.is_some() {
            return Err(ipc_error(
                "PROTOCOL",
                "result header changed after the first page",
            ));
        }
        let partial = self
            .partial
            .as_mut()
            .expect("first page installs a partial result");
        if partial.document.content.len() != page.offset
            || partial.total != page.total_bytes
            || partial.fingerprint != page.fingerprint
            || page.content.len() > partial.total.saturating_sub(page.offset)
        {
            return Err(ipc_error(
                "PROTOCOL",
                "result pages are inconsistent, duplicated or out of order",
            ));
        }
        let end = page.offset + page.content.len();
        if page.done != (end == partial.total) {
            return Err(ipc_error(
                "PROTOCOL",
                "result completion does not match its declared length",
            ));
        }
        partial.document.content.push_str(&page.content);
        if !page.done {
            return Ok(None);
        }
        let partial = self
            .partial
            .take()
            .expect("completed page has a partial result");
        enforce_document_limit(&partial.document)?;
        if fingerprint(&partial.document)? != partial.fingerprint {
            return Err(ipc_error(
                "PROTOCOL",
                "agent result fingerprint does not match its complete payload",
            ));
        }
        self.finished = true;
        Ok(Some(partial.document))
    }
}

#[cfg(test)]
mod tests;
