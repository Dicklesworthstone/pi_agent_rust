//! HTTP operations whose owner survives request construction and streaming.
//!
//! Binding does not create new authority. Transport polling, including idle
//! timeout registration and response-body reads, happens under the captured
//! context rather than the task that happens to poll it. Cancellation stops
//! further polling and drops the transport; it cannot undo a request already
//! accepted by the remote server. A chunk or clean EOF produced during owner
//! cancellation is not published; a transport failure retains its diagnostic.

use super::AgentCx;
use crate::error::{Error, Result};
use crate::http::client::{Client, RequestBuilder, Response};
use futures::{Future, Stream, TryStreamExt};
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

const DEFAULT_BODY_LIMIT: usize = 50 * 1024 * 1024;
// Bound inline collection work independently of the retained byte limit.
// Empty chunks consume scheduling budget too; they consume no byte budget.
const BODY_CHUNKS_PER_YIELD: usize = 64;
type ByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>;
type CancelWait = Pin<Box<dyn Future<Output = ()> + Send>>;

fn check_access(owner: &AgentCx) -> std::io::Result<()> {
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "agent HTTP operations require I/O and timer capabilities",
        ));
    }
    owner.checkpoint().map_err(|_| cancelled())
}

fn cancelled() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "agent HTTP operation cancelled; remote side effects may already have occurred",
    )
}

// The transport may not register with the explicit owner's cancellation lane.
// A cancel-aware receive does so without a polling timer or detached task. The
// sender stays local and alive across the receive, and never publishes: only
// cancellation can complete this wait. Dropping it retires its registration.
async fn cancellation(owner: AgentCx) {
    let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
    let _ = receiver.recv(owner.cx()).await;
    drop(sender);
}

/// A reusable HTTP client carrying an explicit request owner.
/// Cloning it retains that owner; it never captures the cloning task's context.
#[derive(Clone)]
pub struct AgentHttpClient {
    owner: AgentCx,
    client: Client,
}

impl AgentHttpClient {
    pub(super) const fn new(owner: AgentCx, client: Client) -> Self {
        Self { owner, client }
    }

    #[must_use]
    pub fn get(&self, url: &str) -> AgentHttpRequest<'_> {
        AgentHttpRequest::new(self.owner.clone(), self.client.get(url))
    }

    #[must_use]
    pub fn post(&self, url: &str) -> AgentHttpRequest<'_> {
        AgentHttpRequest::new(self.owner.clone(), self.client.post(url))
    }

    #[must_use]
    pub fn delete(&self, url: &str) -> AgentHttpRequest<'_> {
        AgentHttpRequest::new(self.owner.clone(), self.client.delete(url))
    }
}

/// A request builder with no escape hatch that discards its context.
pub struct AgentHttpRequest<'a> {
    owner: AgentCx,
    request: RequestBuilder<'a>,
}

impl<'a> AgentHttpRequest<'a> {
    pub(super) const fn new(owner: AgentCx, request: RequestBuilder<'a>) -> Self {
        Self { owner, request }
    }

    #[must_use]
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.request = self.request.header(key, value);
        self
    }

    pub fn try_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        self.request = self.request.try_header(key, value)?;
        Ok(self)
    }

    #[must_use]
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.request = self.request.body(body);
        self
    }

    pub fn json<T: serde::Serialize>(mut self, payload: &T) -> Result<Self> {
        self.request = self.request.json(payload)?;
        Ok(self)
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.request = self.request.timeout(timeout);
        self
    }

    /// Disable the transport timeout, not the owner's cancellation boundary.
    #[must_use]
    pub fn no_timeout(mut self, ) -> Self {
        self.request = self.request.no_timeout();
        self
    }

    pub async fn send(self) -> Result<AgentHttpResponse> {
        let Self { owner, request } = self;
        check_access(&owner)?;
        let response = {
            let operation = Box::pin(owner.with_current(request.send()));
            let cancellation = Box::pin(cancellation(owner.clone()));
            match futures::future::select(operation, cancellation).await {
                futures::future::Either::Left((response, _)) => response?,
                futures::future::Either::Right(((), pending)) => {
                    let _guard = owner.cx().clone().set_current_restricted();
                    drop(pending);
                    return Err(cancelled().into());
                }
            }
        };
        // Cancellation may have raced the response headers. Drop the response
        // rather than handing an already-cancelled transport to another task.
        check_access(&owner)?;
        Ok(AgentHttpResponse::from_response(owner, response))
    }
}

/// Metadata and a body that retain the same owner as their request.
pub struct AgentHttpResponse {
    owner: AgentCx,
    response: Response,
}

impl AgentHttpResponse {
    /// Adopt a provider response at the stream boundary. This scopes body
    /// consumption only, not the dispatch that already produced the response.
    pub(crate) const fn from_response(owner: AgentCx, response: Response) -> Self {
        Self { owner, response }
    }

    #[must_use]
    pub const fn status(&self) -> u16 {
        self.response.status()
    }

    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        self.response.headers()
    }

    pub fn bytes_stream(self) -> ByteStream {
        Box::pin(OwnedBody::new(self.owner, self.response.bytes_stream()))
    }

    pub async fn bytes_limited(self, limit: usize) -> Result<Vec<u8>> {
        collect_body_limited(self.bytes_stream(), limit).await
    }

    pub async fn text_limited(self, limit: usize) -> Result<String> {
        let bytes = self.bytes_limited(limit).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn text(self) -> Result<String> {
        self.text_limited(DEFAULT_BODY_LIMIT).await
    }
}

/// Collection must yield even if a transport/replay source is always ready.
/// A byte limit alone does not bound an empty-chunk flood, and try_fold can
/// otherwise monopolize one executor poll before a sibling can request abort.
/// Yielding introduces no timer, detached task, data truncation or byte-limit
/// change. A single native read/copy remains cooperative, not preemptible.
async fn collect_body_limited(mut body: ByteStream, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunks = 0;
    while let Some(chunk) = body.try_next().await? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(std::io::Error::other("response body too large").into());
        }
        bytes.extend_from_slice(&chunk);
        chunks += 1;
        if chunks == BODY_CHUNKS_PER_YIELD {
            chunks = 0;
            let mut yielded = false;
            poll_fn(|task| {
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    task.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
        }
    }
    Ok(bytes)
}

struct OwnedBody {
    owner: AgentCx,
    stream: Option<ByteStream>,
    cancellation: Option<CancelWait>,
}

impl OwnedBody {
    fn new(owner: AgentCx, stream: ByteStream) -> Self {
        let cancellation = Box::pin(cancellation(owner.clone()));
        Self {
            owner,
            stream: Some(stream),
            cancellation: Some(cancellation),
        }
    }

    fn retire(&mut self) {
        drop(self.stream.take());
        drop(self.cancellation.take());
    }
}

impl Stream for OwnedBody {
    type Item = std::io::Result<Vec<u8>>;

    fn poll_next(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.stream.is_none() {
            return Poll::Ready(None);
        }
        let _guard = this.owner.cx().clone().set_current_restricted();
        if let Err(error) = check_access(&this.owner) {
            this.retire();
            return Poll::Ready(Some(Err(error)));
        }
        if this
            .cancellation
            .as_mut()
            .expect("active cancellation wait")
            .as_mut()
            .poll(task)
            .is_ready()
        {
            this.retire();
            return Poll::Ready(Some(Err(cancelled())));
        }
        let result = this
            .stream
            .as_mut()
            .expect("active stream")
            .as_mut()
            .poll_next(task);
        // The native poll may complete in the same instant that its owner is
        // cancelled. An SSE consumer can turn this chunk into a terminal
        // success without ever polling the body again. Fence publication here,
        // not just admission above. Also settle a cancelled Pending immediately
        // rather than depending on a second wakeup from the native transport.
        // Keep an actual transport failure, as send() does for header errors.
        if !matches!(result, Poll::Ready(Some(Err(_))))
            && let Err(error) = check_access(&this.owner)
        {
            this.retire();
            return Poll::Ready(Some(Err(error)));
        }
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            // Fusing errors also closes the socket immediately; consumers that
            // poll after a timeout must not receive an infinite error sequence.
            this.retire();
        }
        result
    }
}

impl Drop for OwnedBody {
    fn drop(&mut self) {
        let _guard = self.owner.cx().clone().set_current_restricted();
        self.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{Budget, Cx};
    use futures::StreamExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    #[test]
    fn denied_owner_cannot_borrow_the_callers_http_authority() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let raw = runtime.request_cx_with_budget(Budget::new());
        let owner = {
            let _guard = raw
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            AgentCx::for_current_or_request()
        };
        let client = AgentHttpClient::new(owner, Client::new());
        runtime.block_on(async {
            let caller = Cx::current().expect("caller installed");
            assert!(caller.capabilities().io);
            let error = client
                .get("not a URL")
                .send()
                .await
                .err()
                .expect("denied request");
            assert!(error.to_string().contains("capabilities"));
            assert!(!error.to_string().contains("not a URL"));
            assert_eq!(Cx::current().unwrap().capabilities(), caller.capabilities());
        });
    }

    #[test]
    fn cancelled_request_is_rejected_before_url_parsing_or_dispatch() {
        let owner = AgentCx::for_request();
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("test cancellation"),
        );
        let client = AgentHttpClient::new(owner, Client::new());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let error = runtime
            .block_on(client.get("not a URL").no_timeout().send())
            .err()
            .expect("cancelled request");
        assert!(error.to_string().contains("cancelled"));
        assert!(!error.to_string().contains("not a URL"));
    }

    struct ObservedStream {
        owner_budget: Budget,
        polled: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        fail: bool,
    }

    impl Stream for ObservedStream {
        type Item = std::io::Result<Vec<u8>>;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            assert_eq!(
                Cx::current().expect("owner installed").budget(),
                self.owner_budget
            );
            self.polled.store(true, Ordering::SeqCst);
            if self.fail {
                Poll::Ready(Some(Err(std::io::Error::other("transport failed"))))
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for ObservedStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn body_error_releases_transport_and_is_emitted_only_once() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let budget = Budget::new().with_poll_quota(123);
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(budget));
        let polled = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut body = OwnedBody::new(
            owner,
            Box::pin(ObservedStream {
                owner_budget: budget,
                polled: Arc::clone(&polled),
                dropped: Arc::clone(&dropped),
                fail: true,
            }),
        );
        runtime.block_on(async {
            let caller = Cx::current().unwrap();
            assert!(body.next().await.unwrap().is_err());
            assert!(polled.load(Ordering::SeqCst));
            assert!(dropped.load(Ordering::SeqCst));
            assert!(body.cancellation.is_none());
            assert!(body.next().await.is_none());
            assert_eq!(Cx::current().unwrap().budget(), caller.budget());
        });
    }

    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancellation_wakes_an_idle_body_without_incoming_bytes_or_timer_polls() {
        let budget = Budget::new().with_poll_quota(1000);
        let owner = AgentCx::for_request_with_budget(budget);
        let parent = Cx::for_request_with_budget(Budget::new().with_poll_quota(2000));
        let _parent_guard = parent.clone().set_current_restricted();
        let polled = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut body = OwnedBody::new(
            owner.clone(),
            Box::pin(ObservedStream {
                owner_budget: budget,
                polled: Arc::clone(&polled),
                dropped: Arc::clone(&dropped),
                fail: false,
            }),
        );
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut task = Context::from_waker(&waker);
        assert!(Pin::new(&mut body).poll_next(&mut task).is_pending());
        assert!(polled.load(Ordering::SeqCst));
        assert_eq!(Cx::current().unwrap().budget(), parent.budget());
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("idle cancellation"),
        );
        assert!(
            counter.0.load(Ordering::SeqCst) > 0,
            "owner cancellation must wake the consumer"
        );
        let Poll::Ready(Some(Err(error))) = Pin::new(&mut body).poll_next(&mut task) else {
            panic!("expected terminal cancellation");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(body.cancellation.is_none());
        assert!(matches!(
            Pin::new(&mut body).poll_next(&mut task),
            Poll::Ready(None)
        ));
        assert_eq!(Cx::current().unwrap().budget(), parent.budget());
        assert!(!parent.is_cancel_requested());
    }

    #[test]
    fn successful_eof_retires_the_owner_waiter() {
        let mut body = OwnedBody::new(
            AgentCx::for_request(),
            Box::pin(futures::stream::iter([Ok(vec![0, 255, 1])])),
        );
        let mut task = Context::from_waker(futures::task::noop_waker_ref());
        let Poll::Ready(Some(Ok(bytes))) = Pin::new(&mut body).poll_next(&mut task) else {
            panic!("expected exact bytes");
        };
        assert_eq!(bytes, [0, 255, 1]);
        assert!(matches!(
            Pin::new(&mut body).poll_next(&mut task),
            Poll::Ready(None)
        ));
        assert!(body.stream.is_none());
        assert!(body.cancellation.is_none());
    }

    #[test]
    fn dropping_an_idle_body_retires_its_cancellation_registration() {
        let budget = Budget::new().with_poll_quota(1000);
        let owner = AgentCx::for_request_with_budget(budget);
        let dropped = Arc::new(AtomicBool::new(false));
        let mut body = OwnedBody::new(
            owner.clone(),
            Box::pin(ObservedStream {
                owner_budget: budget,
                polled: Arc::new(AtomicBool::new(false)),
                dropped: Arc::clone(&dropped),
                fail: false,
            }),
        );
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut task = Context::from_waker(&waker);
        assert!(Pin::new(&mut body).poll_next(&mut task).is_pending());
        drop(body);
        assert!(dropped.load(Ordering::SeqCst));
        let before = counter.0.load(Ordering::SeqCst);
        owner.cancel_with(asupersync::types::CancelKind::User, Some("after body drop"));
        assert_eq!(counter.0.load(Ordering::SeqCst), before);
    }

    // A scheduler probe at the real owned-body boundary, not a replacement
    // provider: cancellation happens INSIDE one native stream poll.
    type ReadPoll = Poll<Option<std::io::Result<Vec<u8>>>>;

    struct CancelDuringRead {
        owner: AgentCx,
        outcome: Option<ReadPoll>,
        polls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Stream for CancelDuringRead {
        type Item = std::io::Result<Vec<u8>>;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> ReadPoll {
            let this = self.get_mut();
            assert_eq!(Cx::current().unwrap().budget(), this.owner.budget());
            this.polls.fetch_add(1, Ordering::SeqCst);
            this.owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel inside native read"),
            );
            this.outcome.take().expect("native stream polled only once")
        }
    }

    impl Drop for CancelDuringRead {
        fn drop(&mut self) {
            assert_eq!(Cx::current().unwrap().budget(), self.owner.budget());
            assert!(Cx::current().unwrap().is_cancel_requested());
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn racing_body(outcome: ReadPoll) -> (OwnedBody, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let owner = AgentCx::for_request_with_budget(Budget::new().with_poll_quota(1000));
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let stream = CancelDuringRead {
            owner: owner.clone(),
            outcome: Some(outcome),
            polls: Arc::clone(&polls),
            drops: Arc::clone(&drops),
        };
        (OwnedBody::new(owner, Box::pin(stream)), polls, drops)
    }

    fn assert_cancelled_during_read(outcome: ReadPoll) {
        let parent = Cx::for_request_with_budget(Budget::new().with_poll_quota(2000));
        let _parent_guard = parent.clone().set_current_restricted();
        let (mut body, polls, drops) = racing_body(outcome);
        let mut task = Context::from_waker(Waker::noop());
        let Poll::Ready(Some(Err(error))) = Pin::new(&mut body).poll_next(&mut task) else {
            panic!("cancellation must replace the native result in this poll");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert!(!error.to_string().contains("private-late-payload"));
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(body.stream.is_none());
        assert!(body.cancellation.is_none());
        assert!(matches!(Pin::new(&mut body).poll_next(&mut task), Poll::Ready(None)));
        drop(body);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Cx::current().unwrap().budget(), parent.budget());
        assert!(!parent.is_cancel_requested());
    }

    #[test]
    fn cancellation_during_read_rejects_late_bytes() {
        assert_cancelled_during_read(Poll::Ready(Some(Ok(b"private-late-payload".to_vec()))));
    }

    #[test]
    fn cancellation_during_eof_is_not_clean_completion() {
        assert_cancelled_during_read(Poll::Ready(None));
    }

    #[test]
    fn cancellation_during_pending_finishes_without_another_transport_wakeup() {
        assert_cancelled_during_read(Poll::Pending);
    }

    #[test]
    fn transport_error_racing_cancellation_keeps_its_original_diagnostic() {
        let (mut body, polls, drops) = racing_body(Poll::Ready(Some(Err(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "original reset"),
        ))));
        let mut task = Context::from_waker(Waker::noop());
        let Poll::Ready(Some(Err(error))) = Pin::new(&mut body).poll_next(&mut task) else {
            panic!("native failure must be preserved");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        assert_eq!(error.to_string(), "original reset");
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(body.cancellation.is_none());
        assert!(matches!(Pin::new(&mut body).poll_next(&mut task), Poll::Ready(None)));
    }

    #[test]
    fn precancelled_body_never_polls_the_transport() {
        let (mut body, polls, drops) = racing_body(Poll::Ready(Some(Ok(vec![1, 2, 3]))));
        body.owner.cancel_with(asupersync::types::CancelKind::User, Some("before read"));
        let mut task = Context::from_waker(Waker::noop());
        let Poll::Ready(Some(Err(error))) = Pin::new(&mut body).poll_next(&mut task) else {
            panic!("read must not be admitted");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelled_poller_cannot_cancel_an_unrelated_body_owner() {
        let parent = Cx::for_request();
        parent.cancel_with(asupersync::types::CancelKind::User, Some("unrelated caller"));
        let _parent_guard = parent.set_current_restricted();
        let mut body = OwnedBody::new(
            AgentCx::for_request(),
            Box::pin(futures::stream::iter([Ok(vec![0, 255, 1])])),
        );
        let mut task = Context::from_waker(Waker::noop());
        let Poll::Ready(Some(Ok(bytes))) = Pin::new(&mut body).poll_next(&mut task) else {
            panic!("only the captured owner controls this body");
        };
        assert_eq!(bytes, [0, 255, 1]);
        assert!(matches!(Pin::new(&mut body).poll_next(&mut task), Poll::Ready(None)));
        assert!(Cx::current().unwrap().is_cancel_requested());
    }

    #[test]
    fn sse_parser_never_receives_a_terminal_chunk_produced_during_cancellation() {
        let (body, polls, drops) = racing_body(Poll::Ready(Some(Ok(
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
        ))));
        let mut sse = crate::sse::SseStream::new(body);
        let mut task = Context::from_waker(Waker::noop());
        let Poll::Ready(Some(Err(error))) = Pin::new(&mut sse).poll_next(&mut task) else {
            panic!("terminal SSE success must be rejected at the byte boundary");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(matches!(Pin::new(&mut sse).poll_next(&mut task), Poll::Ready(None)));
    }

    struct ReadyBody {
        chunks: std::vec::IntoIter<std::io::Result<Vec<u8>>>,
        reads: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Stream for ReadyBody {
        type Item = std::io::Result<Vec<u8>>;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> ReadPoll {
            let this = self.get_mut();
            this.reads.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(this.chunks.next())
        }
    }

    impl Drop for ReadyBody {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn ready_body(
        owner: AgentCx,
        chunks: Vec<std::io::Result<Vec<u8>>>,
    ) -> (ByteStream, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let native = ReadyBody {
            chunks: chunks.into_iter(),
            reads: Arc::clone(&reads),
            drops: Arc::clone(&drops),
        };
        (Box::pin(OwnedBody::new(owner, Box::pin(native))), reads, drops)
    }

    #[test]
    fn body_collection_yields_in_bounded_batches_without_changing_binary_output() {
        let chunks: Vec<_> = (0..BODY_CHUNKS_PER_YIELD * 3 + 5)
            .map(|index| vec![u8::try_from(index % 256).unwrap(), 0, 255])
            .collect();
        let expected: Vec<_> = chunks.iter().flatten().copied().collect();
        let (body, reads, drops) = ready_body(
            AgentCx::for_request(),
            chunks.into_iter().map(Ok).collect(),
        );
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut task = Context::from_waker(&waker);
        let mut collect = Box::pin(collect_body_limited(body, expected.len()));
        for batch in 1..=3 {
            assert!(collect.as_mut().poll(&mut task).is_pending());
            assert_eq!(reads.load(Ordering::SeqCst), batch * BODY_CHUNKS_PER_YIELD);
            assert_eq!(counter.0.load(Ordering::SeqCst), batch);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
        let Poll::Ready(Ok(actual)) = collect.as_mut().poll(&mut task) else {
            panic!("finite body must complete after its remaining chunks");
        };
        assert_eq!(actual, expected);
        assert_eq!(reads.load(Ordering::SeqCst), BODY_CHUNKS_PER_YIELD * 3 + 6);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_chunks_yield_at_zero_byte_limit_and_remain_cancellable() {
        let owner = AgentCx::for_request();
        // Finite fixture: without the yield this fails, rather than hanging.
        let (body, reads, drops) = ready_body(
            owner.clone(),
            (0..BODY_CHUNKS_PER_YIELD * 4).map(|_| Ok(Vec::new())).collect(),
        );
        let mut collect = Box::pin(collect_body_limited(body, 0));
        let mut task = Context::from_waker(Waker::noop());
        assert!(collect.as_mut().poll(&mut task).is_pending());
        assert_eq!(reads.load(Ordering::SeqCst), BODY_CHUNKS_PER_YIELD);
        owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel empty body"));
        let Poll::Ready(Err(error)) = collect.as_mut().poll(&mut task) else {
            panic!("cancellation must retire the body before another native read");
        };
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(reads.load(Ordering::SeqCst), BODY_CHUNKS_PER_YIELD);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_byte_limit_and_empty_eof_survive_collection_yields() {
        let (body, _, drops) = ready_body(
            AgentCx::for_request(),
            (0..BODY_CHUNKS_PER_YIELD).map(|_| Ok(vec![42])).collect(),
        );
        let mut collect = Box::pin(collect_body_limited(body, BODY_CHUNKS_PER_YIELD));
        let mut task = Context::from_waker(Waker::noop());
        assert!(collect.as_mut().poll(&mut task).is_pending());
        let Poll::Ready(Ok(bytes)) = collect.as_mut().poll(&mut task) else {
            panic!("exactly-at-limit body must be accepted");
        };
        assert_eq!(bytes, vec![42; BODY_CHUNKS_PER_YIELD]);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let (empty, _, _) = ready_body(AgentCx::for_request(), Vec::new());
        let mut empty = Box::pin(collect_body_limited(empty, 0));
        assert!(matches!(empty.as_mut().poll(&mut task), Poll::Ready(Ok(bytes)) if bytes.is_empty()));
    }

    #[test]
    fn overflow_after_a_yield_is_not_partial_success_and_releases_the_body() {
        let (body, reads, drops) = ready_body(
            AgentCx::for_request(),
            (0..BODY_CHUNKS_PER_YIELD + 2).map(|_| Ok(vec![42])).collect(),
        );
        let mut collect = Box::pin(collect_body_limited(body, BODY_CHUNKS_PER_YIELD));
        let mut task = Context::from_waker(Waker::noop());
        assert!(collect.as_mut().poll(&mut task).is_pending());
        let Poll::Ready(Err(error)) = collect.as_mut().poll(&mut task) else {
            panic!("one byte past the limit must reject the entire collection");
        };
        assert!(error.to_string().contains("response body too large"));
        assert_eq!(reads.load(Ordering::SeqCst), BODY_CHUNKS_PER_YIELD + 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn transport_error_after_a_yield_is_not_lost_or_replaced_with_eof() {
        let mut chunks: Vec<_> = (0..BODY_CHUNKS_PER_YIELD).map(|_| Ok(Vec::new())).collect();
        chunks.push(Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "body reset")));
        let (body, reads, drops) = ready_body(AgentCx::for_request(), chunks);
        let mut collect = Box::pin(collect_body_limited(body, 0));
        let mut task = Context::from_waker(Waker::noop());
        assert!(collect.as_mut().poll(&mut task).is_pending());
        let Poll::Ready(Err(error)) = collect.as_mut().poll(&mut task) else {
            panic!("native failure after scheduling boundary must be preserved");
        };
        assert!(error.to_string().contains("body reset"));
        assert_eq!(reads.load(Ordering::SeqCst), BODY_CHUNKS_PER_YIELD + 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_a_yielded_collection_retires_transport_and_cancel_registration() {
        let owner = AgentCx::for_request();
        let (body, _, drops) = ready_body(
            owner.clone(),
            (0..BODY_CHUNKS_PER_YIELD * 2).map(|_| Ok(Vec::new())).collect(),
        );
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut task = Context::from_waker(&waker);
        let mut collect = Box::pin(collect_body_limited(body, 0));
        assert!(collect.as_mut().poll(&mut task).is_pending());
        drop(collect);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let before = counter.0.load(Ordering::SeqCst);
        owner.cancel_with(asupersync::types::CancelKind::User, Some("after collection drop"));
        assert_eq!(counter.0.load(Ordering::SeqCst), before);
    }

    #[test]
    fn joined_canceller_runs_before_a_ready_body_is_fully_collected() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        runtime.block_on(async {
            let (body, reads, drops) = ready_body(
                owner.clone(),
                (0..BODY_CHUNKS_PER_YIELD * 4).map(|_| Ok(Vec::new())).collect(),
            );
            let collect = collect_body_limited(body, 0);
            let cancel = async {
                assert_eq!(reads.load(Ordering::SeqCst), BODY_CHUNKS_PER_YIELD);
                owner.cancel_with(asupersync::types::CancelKind::User, Some("sibling cancel"));
            };
            let (result, ()) = futures::join!(collect, cancel);
            assert!(result.unwrap_err().to_string().contains("cancelled"));
            assert_eq!(reads.load(Ordering::SeqCst), BODY_CHUNKS_PER_YIELD);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(!Cx::current().unwrap().is_cancel_requested());
        });
    }
}
