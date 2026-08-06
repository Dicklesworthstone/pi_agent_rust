//! Unit and characterization tests for the extensions façade.

// Characterization domains intentionally exercise both public behavior and
// private integration seams. The router imports that full test-only surface;
// each child module then inherits it through its local `super::*`.
use super::*;
use jsonschema::Validator;
use tempfile::tempdir;

use core::{
    capture_tracing_events, create_provider_collision_fixture,
    extension_manager_no_persisted_permissions, run_async,
};
use registration::{MockHostActions, MockSession};
use shared_dispatch::{deny_all_policy, permissive_policy, test_host_call_context};
use ui_protocol::make_host_call_msg;

mod baseline;
mod concurrency;
mod core;
mod enforcement;
mod event_timeouts;
mod exec_security;
mod policy_transition;
mod reactor;
mod registration;
mod risk_math;
mod runtime_parity;
mod security_alerts;
mod shared_dispatch;
mod ui_protocol;
