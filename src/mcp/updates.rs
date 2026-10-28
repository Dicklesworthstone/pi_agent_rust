//! Bounded resource subscriptions and context-change notifications.
//!
//! A server inbox survives reconnect only to report expired subscriptions.
//! Each transport has a unique connection identity, and each HTTP session is
//! checked independently. Notifications never read resources or start work.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::error::{Error, Result};

const MAX_SUBSCRIPTIONS: usize = 64;
const MAX_URI_BYTES: usize = 16 * 1024;
const MAX_TOTAL_URI_BYTES: usize = 64 * 1024;

#[cfg(test)]
mod tests;

fn failure(code: &str, message: &str) -> Error {
    Error::tool("mcp", format!("[{code}] {message}"))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UpdateCapabilities {
    subscribe: bool,
    resources_changed: bool,
    prompts_changed: bool,
}

impl UpdateCapabilities {
    pub(crate) fn from_initialize(result: &Value) -> Result<Self> {
        let capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| failure("MCP_PROTOCOL", "initialize capabilities must be an object"))?;
        let flag = |group: &str, name: &str| -> Result<bool> {
            let Some(value) = capabilities.get(group) else {
                return Ok(false);
            };
            let object = value
                .as_object()
                .ok_or_else(|| failure("MCP_PROTOCOL", "context capabilities must be objects"))?;
            match object.get(name) {
                None => Ok(false),
                Some(Value::Bool(value)) => Ok(*value),
                Some(_) => Err(failure(
                    "MCP_PROTOCOL",
                    "context capability flags must be booleans",
                )),
            }
        };
        Ok(Self {
            subscribe: flag("resources", "subscribe")?,
            resources_changed: flag("resources", "listChanged")?,
            prompts_changed: flag("prompts", "listChanged")?,
        })
    }
}

#[derive(Default)]
struct State {
    connection: u64,
    generation: Option<u64>,
    last_generation: Option<u64>,
    next_mutation: u64,
    capabilities: UpdateCapabilities,
    subscriptions: BTreeMap<String, ResourceSubscription>,
    resources_changed: bool,
    prompts_changed: bool,
    expired: BTreeSet<String>,
    expired_overflow: bool,
}

struct ResourceSubscription {
    subscribed: bool,
    changed: bool,
    mutation: Option<(u64, bool)>,
}

impl State {
    fn expire(&mut self) {
        let mut bytes: usize = self.expired.iter().map(String::len).sum();
        for (uri, subscription) in std::mem::take(&mut self.subscriptions) {
            if !subscription.subscribed || self.expired.contains(&uri) {
                continue;
            }
            if self.expired.len() >= MAX_SUBSCRIPTIONS
                || bytes.saturating_add(uri.len()) > MAX_TOTAL_URI_BYTES
            {
                self.expired_overflow = true;
            } else {
                bytes += uri.len();
                self.expired.insert(uri);
            }
        }
        self.generation = None;
        self.capabilities = UpdateCapabilities::default();
        self.resources_changed = false;
        self.prompts_changed = false;
    }

    fn snapshot(&mut self, connected: bool) -> Value {
        let mut subscriptions = Vec::new();
        let mut updated = Vec::new();
        let capabilities = if connected {
            for (uri, subscription) in &mut self.subscriptions {
                if subscription.subscribed {
                    subscriptions.push(uri.clone());
                    if std::mem::take(&mut subscription.changed) {
                        updated.push(uri.clone());
                    }
                }
            }
            self.capabilities
        } else {
            UpdateCapabilities::default()
        };
        json!({
            "connected": connected,
            "capabilities": {
                "subscribe": capabilities.subscribe,
                "resources_list_changed": capabilities.resources_changed,
                "prompts_list_changed": capabilities.prompts_changed,
            },
            "subscriptions": subscriptions,
            "updated_resources": updated,
            "resources_list_changed": connected && std::mem::take(&mut self.resources_changed),
            "prompts_list_changed": connected && std::mem::take(&mut self.prompts_changed),
            "expired_subscriptions": std::mem::take(&mut self.expired),
            "expired_overflow": std::mem::take(&mut self.expired_overflow),
        })
    }
}

/// One manager-owned, bounded inbox for one configured server. Native
/// transports bind their own connection identity before initialization.
#[derive(Default)]
pub struct ContextUpdates {
    state: Mutex<State>,
}

impl ContextUpdates {
    pub(crate) fn connect(self: &Arc<Self>) -> Result<ContextUpdateConnection> {
        let mut state = lock(&self.state);
        let connection = state.connection.checked_add(1).ok_or_else(|| {
            failure(
                "MCP_REQUEST_LIMIT",
                "context connection identities are exhausted",
            )
        })?;
        state.expire();
        state.connection = connection;
        state.last_generation = None;
        Ok(ContextUpdateConnection {
            inbox: Arc::clone(self),
            connection,
        })
    }

    /// Consume only expiry notices when there is no published connection.
    /// A private handshake may already own the inbox, so do not retire it or
    /// consume its notifications before the manager publishes that transport.
    pub(crate) fn poll_disconnected(&self) -> Value {
        lock(&self.state).snapshot(false)
    }

    pub(crate) fn clear(&self) {
        let mut state = lock(&self.state);
        state.expire();
        // Also fence a private handshake which has not initialized yet.
        state.last_generation = Some(u64::MAX);
        state.expired.clear();
        state.expired_overflow = false;
    }
}

/// Connection identity retained by a native transport. Old readers cannot
/// address a replacement connection even when both use generation zero.
pub struct ContextUpdateConnection {
    inbox: Arc<ContextUpdates>,
    connection: u64,
}

impl ContextUpdateConnection {
    pub(crate) fn belongs_to(&self, inbox: &Arc<ContextUpdates>) -> bool {
        Arc::ptr_eq(&self.inbox, inbox)
    }

    pub(crate) fn initialize(
        &self,
        generation: u64,
        capabilities: UpdateCapabilities,
    ) -> Result<()> {
        let mut state = lock(&self.inbox.state);
        if state.connection != self.connection {
            return Err(failure(
                "MCP_TRANSPORT_SUPERSEDED",
                "context connection was replaced",
            ));
        }
        if state.generation == Some(generation) {
            return if state.capabilities == capabilities {
                Ok(())
            } else {
                Err(failure(
                    "MCP_PROTOCOL",
                    "context capabilities changed within a protocol session",
                ))
            };
        }
        if state
            .last_generation
            .is_some_and(|previous| generation <= previous)
        {
            return Err(failure(
                "MCP_SESSION_CHANGED",
                "context protocol session was retired",
            ));
        }
        state.expire();
        state.generation = Some(generation);
        state.last_generation = Some(generation);
        state.capabilities = capabilities;
        Ok(())
    }

    pub(crate) fn retire(&self, generation: u64) {
        let mut state = lock(&self.inbox.state);
        if state.connection == self.connection
            && (state.generation == Some(generation)
                || (state.generation.is_none() && state.last_generation.is_none()))
        {
            state.expire();
            state.last_generation = Some(generation);
        }
    }

    pub(crate) fn publish(&self, message: &Value, generation: u64) {
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || message.get("id").is_some()
            || message.get("result").is_some()
            || message.get("error").is_some()
            || message
                .get("params")
                .is_some_and(|params| !params.is_object())
        {
            return;
        }
        let mut state = lock(&self.inbox.state);
        if state.connection != self.connection || state.generation != Some(generation) {
            return;
        }
        match message.get("method").and_then(Value::as_str) {
            Some("notifications/resources/list_changed")
                if state.capabilities.resources_changed =>
            {
                state.resources_changed = true;
            }
            Some("notifications/prompts/list_changed") if state.capabilities.prompts_changed => {
                state.prompts_changed = true;
            }
            Some("notifications/resources/updated") if state.capabilities.subscribe => {
                let Some(uri) = message.pointer("/params/uri").and_then(Value::as_str) else {
                    return;
                };
                if let Some(subscription) = state.subscriptions.get_mut(uri) {
                    subscription.changed = true;
                }
            }
            _ => {}
        }
    }

    pub(crate) fn poll(&self, generation: u64, alive: bool) -> Result<Value> {
        let mut state = lock(&self.inbox.state);
        let owns_generation = match state.generation {
            Some(current) => generation == current,
            // HTTP increments its wire generation as it expires a session.
            // That disconnected owner may drain the preceding expiry, but a
            // reader captured before a newer session retired may not do so.
            None => state
                .last_generation
                .is_some_and(|retired| generation >= retired),
        };
        if state.connection != self.connection || !owns_generation {
            return Err(failure(
                "MCP_SESSION_CHANGED",
                "context update session changed before polling",
            ));
        }
        let current = state.generation == Some(generation);
        if current && !alive {
            state.expire();
        }
        Ok(state.snapshot(current && alive))
    }

    /// Reserve before dispatch so a fast server's update cannot outrun its
    /// subscribe acknowledgement. `None` is an already-satisfied operation.
    pub(crate) fn begin(
        &self,
        generation: u64,
        uri: &str,
        subscribe: bool,
    ) -> Result<Option<UpdateMutation>> {
        if uri.is_empty() || uri.len() > MAX_URI_BYTES || uri.chars().any(char::is_control) {
            return Err(failure(
                "MCP_REQUEST_INVALID",
                "resource URI must be nonempty, bounded, and contain no control characters",
            ));
        }
        let mut state = lock(&self.inbox.state);
        if state.connection != self.connection || state.generation != Some(generation) {
            return Err(failure(
                "MCP_SESSION_CHANGED",
                "resource subscription session is no longer current",
            ));
        }
        if !state.capabilities.subscribe {
            return Err(failure(
                "MCP_SUBSCRIPTION_UNSUPPORTED",
                "server did not advertise resource subscriptions",
            ));
        }
        if let Some(existing) = state.subscriptions.get(uri) {
            if existing.mutation.is_some() {
                return Err(failure(
                    "MCP_SUBSCRIPTION_BUSY",
                    "a subscription operation for this URI is already pending",
                ));
            }
            if subscribe && existing.subscribed {
                return Ok(None);
            }
        } else if !subscribe {
            return Ok(None);
        }
        if subscribe
            && (state.subscriptions.len() >= MAX_SUBSCRIPTIONS
                || state
                    .subscriptions
                    .keys()
                    .map(String::len)
                    .sum::<usize>()
                    .saturating_add(uri.len())
                    > MAX_TOTAL_URI_BYTES)
        {
            return Err(failure(
                "MCP_SUBSCRIPTION_LIMIT",
                "resource subscriptions exceed the count or total URI byte limit",
            ));
        }
        let mutation = state.next_mutation.checked_add(1).ok_or_else(|| {
            failure(
                "MCP_REQUEST_LIMIT",
                "resource subscription operation identities are exhausted",
            )
        })?;
        state.next_mutation = mutation;
        if subscribe {
            state.subscriptions.insert(
                uri.to_string(),
                ResourceSubscription {
                    subscribed: false,
                    changed: false,
                    mutation: Some((mutation, true)),
                },
            );
        } else {
            state
                .subscriptions
                .get_mut(uri)
                .expect("subscribed resource")
                .mutation = Some((mutation, false));
        }
        Ok(Some(UpdateMutation {
            inbox: Arc::clone(&self.inbox),
            connection: self.connection,
            generation,
            uri: uri.to_string(),
            mutation,
            subscribe,
            committed: false,
        }))
    }
}

pub(crate) struct UpdateMutation {
    inbox: Arc<ContextUpdates>,
    connection: u64,
    generation: u64,
    uri: String,
    mutation: u64,
    subscribe: bool,
    committed: bool,
}

impl UpdateMutation {
    pub(crate) fn commit(mut self) -> Result<()> {
        let mut state = lock(&self.inbox.state);
        if state.connection != self.connection || state.generation != Some(self.generation) {
            return Err(failure(
                "MCP_SESSION_CHANGED",
                "resource subscription acknowledgement belongs to an expired session",
            ));
        }
        let subscription = state
            .subscriptions
            .get_mut(&self.uri)
            .filter(|subscription| subscription.mutation == Some((self.mutation, self.subscribe)))
            .ok_or_else(|| {
                failure(
                    "MCP_SESSION_CHANGED",
                    "resource subscription operation was retired",
                )
            })?;
        if self.subscribe {
            subscription.subscribed = true;
            subscription.mutation = None;
        } else {
            state.subscriptions.remove(&self.uri);
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for UpdateMutation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = lock(&self.inbox.state);
        if state.connection != self.connection || state.generation != Some(self.generation) {
            return;
        }
        let Some(subscription) = state
            .subscriptions
            .get_mut(&self.uri)
            .filter(|subscription| subscription.mutation == Some((self.mutation, self.subscribe)))
        else {
            return;
        };
        if self.subscribe {
            state.subscriptions.remove(&self.uri);
        } else {
            subscription.mutation = None;
        }
    }
}
