//! Keep live turns on their initiating owner and drain cooperative aborts.
//!
//! The polling task may change between wakeups. It must not replace the
//! request's cancellation lane, clock, budget or ambient capabilities. A turn
//! built outside any scoped context binds once on its first poll instead.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::agent::AgentEvent;
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::model::{AssistantMessage, ContentBlock, ImageContent, TextContent, UserContent};

use super::{ControllableSession, ControlledTurn, SessionControlHandle, control_error};

impl ControllableSession {
    /// Send text and images through the same live-control and recovery boundary.
    /// Empty text with images is image-only input; no images uses the ordinary
    /// text path, without manufacturing an empty block or changing input hooks.
    pub fn prompt_with_images(
        &mut self,
        input: String,
        images: Vec<ImageContent>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        self.prompt_with_images_with_control(input, images, move |_, event| on_event(event))
    }

    /// Image-bearing counterpart of [`Self::prompt_with_control`]. The handle
    /// remains bound to one turn even when the session is reused for later
    /// prompts. Unclaimed queued input is recoverable, never replayed silently.
    pub fn prompt_with_images_with_control(
        &mut self,
        input: String,
        images: Vec<ImageContent>,
        on_event: impl Fn(&SessionControlHandle, AgentEvent) + Send + Sync + 'static,
    ) -> Result<ControlledTurn<impl Future<Output = Result<AssistantMessage>> + '_>> {
        let content = if images.is_empty() {
            UserContent::Text(input)
        } else {
            let mut blocks = Vec::with_capacity(images.len().saturating_add(1));
            if !input.is_empty() {
                blocks.push(ContentBlock::Text(TextContent::new(input)));
            }
            blocks.extend(images.into_iter().map(ContentBlock::Image));
            UserContent::Blocks(blocks)
        };
        self.prompt_input_with_control(content, on_event)
    }
}

pub(super) type CancelWait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Register with the explicit owner's cancellation lane without a task or
/// polling timer. The local sender never publishes and remains alive until
/// cancellation completes the receive; dropping the wait retires its waker.
pub(super) fn cancellation(owner: AgentCx) -> CancelWait {
    Box::pin(async move {
        let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
        let _ = receiver.recv(owner.cx()).await;
        drop(sender);
    })
}

pub(super) struct OwnedTurn<F> {
    owner: Option<AgentCx>,
    future: Option<Pin<Box<F>>>,
    cancellation: Option<CancelWait>,
    control: SessionControlHandle,
    started: bool,
}

impl<F> OwnedTurn<F> {
    /// This constructor is synchronous: an explicit initiating context is
    /// captured before the future can be moved into a different task.
    pub(super) fn new(future: F, control: SessionControlHandle) -> Self {
        let owner = asupersync::Cx::current().map(AgentCx::from_cx);
        let cancellation = owner.clone().map(cancellation);
        Self {
            owner,
            future: Some(Box::pin(future)),
            cancellation,
            control,
            started: false,
        }
    }

    fn retire(&mut self) {
        // Retire while the owner is installed, including a captured TurnGuard
        // on the unpolled path. Native destructors must not inherit the poller.
        drop(self.future.take());
        drop(self.cancellation.take());
    }
}

impl<F: Future<Output = Result<AssistantMessage>>> Future for OwnedTurn<F> {
    type Output = Result<AssistantMessage>;

    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        assert!(
            this.future.is_some(),
            "a completed owned turn cannot be polled again"
        );
        if this.owner.is_none() {
            let owner = AgentCx::for_current_or_request();
            this.cancellation = Some(cancellation(owner.clone()));
            this.owner = Some(owner);
        }
        let owner = this.owner.as_ref().expect("turn owner bound").clone();
        let _current = owner.cx().clone().set_current_restricted();

        let owner_cancelled = owner.checkpoint().is_err()
            || this
                .cancellation
                .as_mut()
                .is_some_and(|wait| wait.as_mut().poll(task).is_ready());
        if owner_cancelled {
            this.control.abort();
            drop(this.cancellation.take());
        }
        if !this.started && !this.control.snapshot().accepting_input {
            // The SDK's native prompt starts with MCP synchronization and
            // primary-model restoration, before it reaches its abort-aware
            // agent loop. Do not dispatch any of that for an unused abort.
            this.retire();
            return Poll::Ready(Err(control_error(
                "SESSION_CONTROL_CANCELLED",
                "turn cancelled before execution; no prompt or continuation was dispatched",
            )));
        }

        this.started = true;
        let result = this
            .future
            .as_mut()
            .expect("running turn retains its native future")
            .as_mut()
            .poll(task);
        if result.is_ready() {
            this.retire();
        }
        // Once dispatched, cancellation only seals admission and requests the
        // native abort signal. Keep polling this SAME future to completion.
        // In particular, retain typed persistence errors and partial results;
        // neither cancellation nor dropping can promise to roll back effects.
        result
    }
}

impl<F> Drop for OwnedTurn<F> {
    fn drop(&mut self) {
        let _current = self
            .owner
            .as_ref()
            .map(|owner| owner.cx().clone().set_current_restricted());
        self.retire();
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod native_input_tests;
