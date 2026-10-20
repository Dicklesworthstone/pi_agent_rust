//! Fresh per-call kernels over the ordinary native eval lifecycle.
//!
//! Isolation is of interpreter state, not of files, processes or credentials.
//! The private executor shares admission and the live session resolver, never
//! retained slots. No alternate compiler, bridge, capture or timeout path.

use super::{EvalTool, output::Update};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KernelMode {
    Session,
    PerCall,
}

impl KernelMode {
    pub(super) fn parse(input: &Value) -> Result<Self> {
        match input.get("kernel_mode") {
            None => Ok(Self::Session),
            Some(Value::String(mode)) if mode == "session" => Ok(Self::Session),
            Some(Value::String(mode)) if mode == "per_call" => Ok(Self::PerCall),
            _ => Err(Error::tool("eval", "EVAL_INPUT: kernel_mode must be session or per_call")),
        }
    }
}

impl EvalTool {
    /// Copy captured configuration, not ambient environment or retained state.
    /// This does not launch anything. The normal lifecycle still binds on first
    /// poll and rejects queued work whose conversation changed before admission.
    pub(super) fn isolated_executor(&self) -> Self {
        Self {
            cwd: self.cwd.clone(),
            python_path: self.python_path.clone(),
            python_environment: self.python_environment.clone(),
            kernel: Mutex::new(None),
            js: Mutex::new(None),
            session_scope: self.session_scope.clone(),
            retained_sessions: Mutex::new(super::session::RetainedSessions::default()),
            execution: Arc::clone(&self.execution),
            judge: self.judge.clone(),
        }
    }

    pub(super) async fn execute_with_mode(
        &self,
        input: Value,
        on_update: Option<Update>,
        protect: Option<&crate::judgment::Protect<'_>>,
        host: Option<&dyn crate::tools::ToolInvocationHost>,
    ) -> Result<ToolOutput> {
        if KernelMode::parse(&input)? == KernelMode::Session {
            return self.execute_cell(input, on_update, protect, host).await;
        }
        let owner = AgentCx::for_current_or_request();
        let private = self.isolated_executor();
        let on_update: Option<Update> = on_update.map(|callback| {
            Box::new(move |mut update: crate::tools::ToolUpdate| {
                annotate(&mut update.details);
                callback(update);
            }) as Update
        });
        let outcome = private.execute_cell(input, on_update, protect, host).await;
        // Retire even a successful or ordinary-error kernel before handing
        // out the result. Abandonment drops the private executor and checked-
        // out runner too; neither has access to the parent's kernel slots.
        {
            let _current = owner.cx().clone().set_current_restricted();
            drop(private);
        }
        let mut output = outcome.map_err(|error| Error::tool("eval", format!(
            "{error}; EVAL_PER_CALL: private interpreter state is not retained; session kernels untouched"
        )))?;
        annotate(&mut output.details);
        // A clean soft stop normally keeps Python's state. This private
        // executor was deliberately retired instead; do not claim reuse.
        if output.details.as_ref().is_some_and(|d| d["softTimeout"] == true) {
            if let Some(Value::Object(details)) = output.details.as_mut() {
                details.insert("stateReset".into(), json!(true));
            }
            output.content.insert(0, ContentBlock::Text(TextContent::new(
                "EVAL_PER_CALL: soft-stopped private kernel retired; no variables are retained. Session kernels are unchanged.\n"
            )));
        }
        Ok(output)
    }
}

fn annotate(details: &mut Option<Value>) {
    let details = details.get_or_insert_with(|| json!({}));
    if let Some(details) = details.as_object_mut() {
        details.insert("kernelMode".into(), json!("per_call"));
        details.insert("stateRetained".into(), json!(false));
    }
}

#[cfg(test)]
mod tests;
