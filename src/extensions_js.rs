//! QuickJS runtime scaffolding for JS-compatible extensions.
//!
//! This is a minimal foundation for the bd-1f5 workstream. It provides:
//! - Async QuickJS runtime + context creation
//! - A stub `pi` global object (hostcalls not yet wired)
//! - Helpers to evaluate scripts and drain the job queue

use crate::error::{Error, Result};
use rquickjs::function::Func;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, IntoJs, Object, Value};

pub struct QuickJsRuntime {
    runtime: AsyncRuntime,
    context: AsyncContext,
}

impl QuickJsRuntime {
    pub async fn new() -> Result<Self> {
        let runtime = AsyncRuntime::new().map_err(map_js_error)?;
        let context = AsyncContext::full(&runtime).await.map_err(map_js_error)?;
        let instance = Self { runtime, context };
        instance.install_pi_stub().await?;
        Ok(instance)
    }

    pub async fn eval(&self, source: &str) -> Result<()> {
        self.context
            .with(|ctx| ctx.eval::<(), _>(source))
            .await
            .map_err(map_js_error)?;
        Ok(())
    }

    pub async fn eval_file(&self, path: &std::path::Path) -> Result<()> {
        self.context
            .with(|ctx| ctx.eval_file::<(), _>(path))
            .await
            .map_err(map_js_error)?;
        Ok(())
    }

    pub async fn run_pending_jobs(&self) -> Result<()> {
        loop {
            let ran = self
                .runtime
                .execute_pending_job()
                .await
                .map_err(|err| Error::extension(format!("QuickJS job: {err}")))?;
            if !ran {
                break;
            }
        }
        Ok(())
    }

    pub async fn run_until_idle(&self) -> Result<()> {
        self.runtime.idle().await;
        Ok(())
    }

    async fn install_pi_stub(&self) -> Result<()> {
        self.context
            .with(|ctx| {
                let global = ctx.globals();
                let pi = Object::new(ctx)?;

                pi.set(
                    "tool",
                    Func::from(
                        |ctx: Ctx<'_>, _name: String, _input: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(ctx, "pi.tool"))
                        },
                    ),
                )?;
                pi.set(
                    "exec",
                    Func::from(
                        |ctx: Ctx<'_>, _cmd: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(ctx, "pi.exec"))
                        },
                    ),
                )?;
                pi.set(
                    "http",
                    Func::from(|ctx: Ctx<'_>, _req: Value| -> rquickjs::Result<Value> {
                        Err(throw_unimplemented(ctx, "pi.http"))
                    }),
                )?;
                pi.set(
                    "session",
                    Func::from(
                        |ctx: Ctx<'_>, _op: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(ctx, "pi.session"))
                        },
                    ),
                )?;
                pi.set(
                    "ui",
                    Func::from(
                        |ctx: Ctx<'_>, _op: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(ctx, "pi.ui"))
                        },
                    ),
                )?;
                pi.set(
                    "events",
                    Func::from(
                        |ctx: Ctx<'_>, _op: String, _args: Value| -> rquickjs::Result<Value> {
                            Err(throw_unimplemented(ctx, "pi.events"))
                        },
                    ),
                )?;

                global.set("pi", pi)?;
                Ok(())
            })
            .await
            .map_err(map_js_error)?;
        Ok(())
    }
}

fn throw_unimplemented(ctx: Ctx<'_>, name: &str) -> rquickjs::Error {
    let message = format!("{name} is not wired yet");
    match message.into_js(&ctx) {
        Ok(value) => ctx.throw(value),
        Err(err) => err,
    }
}

fn map_js_error(err: rquickjs::Error) -> Error {
    Error::extension(format!("QuickJS: {err}"))
}
