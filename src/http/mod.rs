pub mod client;
pub mod sse;

// Re-export timeout configuration function
pub use client::effective_http_timeout;

// Test modules - only compile when asupersync is working
// #[cfg(test)]
// mod test_api;
// #[cfg(test)]
// mod test_asupersync;
