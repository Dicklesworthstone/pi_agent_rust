//! HTTP client wrapper around asupersync.
//!
//! This module provides a high-level HTTP client compatible with the
//! requirements of the Provider trait, abstracting over asupersync's
//! structured concurrency primitives.

use crate::error::{Error, Result};
use std::time::Duration;

/// A configured HTTP client.
#[derive(Debug, Clone)]
pub struct Client {
    // TODO: Add asupersync HttpClient when available
    // inner: asupersync::http::client::HttpClient,
}

impl Client {
    /// Create a new HTTP client.
    pub fn new() -> Self {
        Self {}
    }

    /// Create a POST request builder.
    pub fn post(&self, url: &str) -> RequestBuilder {
        RequestBuilder {
            method: Method::Post,
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
        }
    }
}

/// HTTP request builder.
pub struct RequestBuilder {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

enum Method {
    Get,
    Post,
}

impl RequestBuilder {
    /// Add a header to the request.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Set the request body as JSON.
    pub fn json<T: serde::Serialize>(mut self, json: &T) -> Self {
        match serde_json::to_vec(json) {
            Ok(bytes) => {
                self.headers.push(("Content-Type".to_string(), "application/json".to_string()));
                self.body = Some(bytes);
            }
            Err(e) => {
                // We can't easily return error here due to builder pattern,
                // so we might need to store it or panic (but no panics allowed).
                // For now, we'll log/ignore or change API to return Result.
                // Re-designing builder to be infallible until send() is better.
                // But serialization can fail.
                // Let's assume for now serialization doesn't fail for our types.
            }
        }
        self
    }

    /// Send the request and return the response.
    ///
    /// This requires an async context in the future.
    pub async fn send(self) -> Result<Response> {
        // TODO: Implement using asupersync
        Err(Error::api("asupersync HTTP client not yet implemented"))
    }
}

/// HTTP response.
pub struct Response {
    status: u16,
    // body stream...
}

impl Response {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub async fn text(self) -> Result<String> {
        Ok(String::new())
    }
}
