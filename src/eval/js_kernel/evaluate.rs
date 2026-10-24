//! Choose script versus top-level-await grammar without executing a probe.
//!
//! The Function constructor parses a body but never executes it. Its grammar
//! accepts ordinary script statements (and a few extra constructs, such as
//! return, which the actual script evaluation still rejects). Only a syntax
//! failure in this non-executing probe selects QuickJS's async eval mode.
//! Never retry after evaluating user code: SyntaxError can be thrown at runtime.

use rquickjs::{Ctx, Function, Persistent, Value};

mod imports;

pub(super) struct Evaluator {
    // Capture the intrinsic before the first cell. A cell replacing the global
    // Function must not turn our parse-only probe into arbitrary execution.
    compile_body: Persistent<Function<'static>>,
}

impl Evaluator {
    pub(super) fn new(ctx: &Ctx<'_>) -> rquickjs::Result<Self> {
        let constructor: Function<'_> = ctx.globals().get("Function")?;
        Ok(Self {
            compile_body: Persistent::save(ctx, constructor),
        })
    }

    pub(super) fn evaluate<'js>(
        &self,
        ctx: &Ctx<'js>,
        code: &str,
        filename: Option<&str>,
    ) -> rquickjs::Result<Value<'js>> {
        let constructor = self.compile_body.clone().restore(ctx)?;
        // Hashbangs are valid only at the start of a script, not a function
        // body. Remove one from the probe only; evaluate the original bytes.
        let probe = code.strip_prefix('\u{feff}').unwrap_or(code);
        let probe = if probe.starts_with("#!") {
            probe.find(['\r', '\n']).map_or("", |end| &probe[end..])
        } else {
            probe
        };
        let needs_async = match constructor.call::<_, Function<'js>>((probe,)) {
            Ok(_) => false,
            Err(error) if error.is_exception() => {
                let caught = ctx.catch();
                if !caught.as_exception().is_some_and(|exception| {
                    exception
                        .get::<_, String>("name")
                        .is_ok_and(|name| name == "SyntaxError")
                }) {
                    return Err(ctx.throw(caught));
                }
                true
            }
            Err(error) => return Err(error),
        };
        // Lower only before the first evaluation. A runtime SyntaxError must
        // never cause a second execution of a cell or its dependencies.
        let prepared = if needs_async {
            imports::prepare(code)
                .map_err(|error| rquickjs::Exception::throw_message(ctx, error))?
        } else {
            None
        };
        let code = prepared.as_deref().unwrap_or(code);
        let mut options = rquickjs::context::EvalOptions::default();
        options.promise = needs_async;
        // Preserve the real origin for stacks and dynamic imports, including
        // functions defined by this script and invoked in later cells.
        options.filename = filename.map(str::to_owned);
        ctx.eval_with_options(code.as_bytes(), options)
    }
}
