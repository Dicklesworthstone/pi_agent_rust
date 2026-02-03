pub mod client;
pub mod sse {
    pub use crate::sse::{SseEvent, SseParser, SseStream};
}

// Test modules - only compile when asupersync is working
// #[cfg(test)]
// mod test_api;
// #[cfg(test)]
// mod test_asupersync;
