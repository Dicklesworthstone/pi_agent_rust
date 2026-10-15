//! Native pointer sequences on one resolved node, never JavaScript clicks.
//!
//! Double-click steps retain the original document and node. Every admitted
//! press gets one release attempt; failures never replay an earlier click.

use super::{Cdp, Document, document, element_call, frame_input};
use crate::agent_cx::AgentCx;
use crate::browser::{output, required};
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
        }
    }

    const fn mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 4,
            Self::Right => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Options {
    button: Button,
    count: u8,
    modifiers: u8,
}

fn usage() -> Error {
    Error::tool(
        "browser",
        "BROWSER_POINTER_USAGE: click accepts button (left/middle/right), click_count (1 or 2; double-click requires left), and distinct modifiers (Alt/Control/Meta/Shift)",
    )
}

impl Options {
    fn parse(args: &Value) -> Result<Self> {
        let button = match args.get("button") {
            None => Button::Left,
            Some(Value::String(value)) => match value.as_str() {
                "left" => Button::Left,
                "middle" => Button::Middle,
                "right" => Button::Right,
                _ => return Err(usage()),
            },
            _ => return Err(usage()),
        };
        let count = match args.get("click_count") {
            None => 1,
            Some(value) => match value.as_u64() {
                Some(1) => 1,
                Some(2) => 2,
                _ => return Err(usage()),
            },
        };
        if count == 2 && button != Button::Left {
            return Err(usage());
        }
        let mut modifiers = 0;
        if let Some(value) = args.get("modifiers") {
            let keys = value.as_array().filter(|keys| keys.len() <= 4).ok_or_else(usage)?;
            for key in keys {
                let bit = match key.as_str() {
                    Some("Alt") => 1,
                    Some("Control") => 2,
                    Some("Meta") => 4,
                    Some("Shift") => 8,
                    _ => return Err(usage()),
                };
                if modifiers & bit != 0 {
                    return Err(usage());
                }
                modifiers |= bit;
            }
        }
        Ok(Self { button, count, modifiers })
    }

    fn event(self, x: f64, y: f64, number: u8, pressed: bool) -> Value {
        json!({
            "type":if pressed { "mousePressed" } else { "mouseReleased" },
            "x":x,"y":y,"button":self.button.name(),
            "buttons":if pressed { self.button.mask() } else { 0 },
            "clickCount":number,"modifiers":self.modifiers,
        })
    }
}

/// Public preflight precedes launch, connection and the explicit mock backend.
/// Pointer options on any other action are errors, never silently ignored.
pub(in crate::browser) fn validate_input(args: &Value) -> Result<bool> {
    let explicit = ["button", "click_count", "modifiers"]
        .iter().any(|name| args.get(*name).is_some());
    if explicit {
        if args.get("action").and_then(Value::as_str) != Some("click") {
            return Err(usage());
        }
        Options::parse(args)?;
    }
    Ok(explicit)
}

async fn verify_document(owner: &AgentCx, cdp: &mut Cdp, expected: &Document) -> Result<()> {
    owner.checkpoint().map_err(|_| Error::tool("browser", "BROWSER_POINTER_CANCELLED"))?;
    if document(owner, cdp).await? != *expected {
        return Err(Error::tool("browser", "BROWSER_POINTER_STALE: page changed during click sequence"));
    }
    Ok(())
}

async fn point(owner: &AgentCx, cdp: &mut Cdp, id: u64) -> Result<(f64, f64)> {
    if cdp.frame_selected() {
        // Includes the existing transformed/OOP-frame projection and hit tests.
        return frame_input::click_point(owner, cdp, id).await;
    }
    let value = element_call(owner, cdp, id, "point", json!({})).await?;
    let coordinate = |key: &str| {
        value[key].as_f64()
            .filter(|value| value.is_finite() && (0.0..=1_000_000.0).contains(value))
            .ok_or_else(|| Error::tool("browser", "BROWSER_POINTER_GEOMETRY: invalid clickable point"))
    };
    Ok((coordinate("x")?, coordinate("y")?))
}

pub(super) async fn click(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    id: u64,
    args: &Value,
) -> Result<ToolOutput> {
    let options = Options::parse(args)?;
    let selector = required(args, "selector")?;
    let expected = document(owner, cdp).await?;
    let original_page = cdp.page_session_id()?.to_owned();
    let mut completed = 0_u8;
    let sequence: Result<()> = async {
        // Bring ordinary main-document clicks to the front too. Background
        // tabs must not rely on incidental focus left by an earlier call.
        cdp.command(owner, "Page.bringToFront", json!({})).await?;
        verify_document(owner, cdp, &expected).await?;
        for number in 1..=options.count {
            // Never re-resolve a selector after page handlers run. If the
            // original node disappeared, stop rather than clicking its replacement.
            let (x, y) = point(owner, cdp, id).await?;
            verify_document(owner, cdp, &expected).await?;
            let pressed = cdp.command(
                owner, "Input.dispatchMouseEvent", options.event(x, y, number, true),
            ).await;
            let released = if pressed.is_ok() {
                // The ordinary route must keep its explicit expected-dialog
                // handling. It is still this connection's original page session.
                cdp.command(
                    owner, "Input.dispatchMouseEvent", options.event(x, y, number, false),
                ).await
            } else {
                // A lost acknowledgement does not prove the press was rejected.
                // Attempt one release on the original page, not a newly selected
                // frame. The same owner/deadline applies: cleanup is best effort.
                cdp.frame_command(
                    owner, &original_page, "Input.dispatchMouseEvent",
                    options.event(x, y, number, false),
                ).await
            };
            pressed?;
            released?;
            completed += 1;
            if number < options.count {
                verify_document(owner, cdp, &expected).await?;
            }
        }
        Ok(())
    }.await;
    if let Err(error) = sequence {
        return Err(Error::tool("browser", format!(
            "BROWSER_POINTER_INTERRUPTED: {completed} complete click pairs acknowledged; side effects may already have occurred. No click was retried. {error}"
        )));
    }
    Ok(output(
        format!("Dispatched {} {} click(s) to {selector} in tab {tab}; application outcome is not verified", options.count, options.button.name()),
        json!({
            "action":"click","selector":selector,"tab":tab,"backend":"cdp",
            "button":options.button.name(),"click_count":options.count,
            "modifier_mask":options.modifiers,"input_method":"native-pointer",
            "dispatch_acknowledged":true,"delivery_verified":false,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(case: &str) {
        eprintln!("{}", json!({"suite":"browser_pointer","case":case,"phase":"start"}));
    }

    #[test]
    fn defaults_and_all_three_native_buttons_have_the_correct_masks() {
        log("button-masks");
        assert_eq!(Options::parse(&json!({})).unwrap(), Options { button:Button::Left, count:1, modifiers:0 });
        for (name, mask) in [("left",1), ("middle",4), ("right",2)] {
            let options = Options::parse(&json!({"button":name})).unwrap();
            assert_eq!(options.event(10.0, 20.0, 1, true), json!({
                "type":"mousePressed","x":10.0,"y":20.0,"button":name,
                "buttons":mask,"clickCount":1,"modifiers":0,
            }));
            assert_eq!(options.event(10.0, 20.0, 1, false)["buttons"], 0);
        }
    }

    #[test]
    fn double_click_counts_advance_but_are_not_an_implicit_retry() {
        log("double-click-events");
        let options = Options::parse(&json!({"click_count":2,"modifiers":["Shift"]})).unwrap();
        let events: Vec<_> = (1..=options.count).flat_map(|n| [options.event(1.0,2.0,n,true),options.event(1.0,2.0,n,false)]).collect();
        assert_eq!(events.iter().map(|event|event["clickCount"].clone()).collect::<Vec<_>>(), vec![json!(1),json!(1),json!(2),json!(2)]);
        assert_eq!(events.iter().map(|event|event["buttons"].clone()).collect::<Vec<_>>(), vec![json!(1),json!(0),json!(1),json!(0)]);
        assert!(events.iter().all(|event| event["modifiers"] == 8));
    }

    #[test]
    fn modifiers_are_distinct_typed_and_never_insert_shortcut_text() {
        log("modifier-validation");
        let options = Options::parse(&json!({"modifiers":["Alt","Control","Meta","Shift"]})).unwrap();
        assert_eq!(options.modifiers, 15);
        for value in [json!(["Shift","Shift"]), json!(["ctrl"]), json!([1]), json!("Shift"), Value::Null] {
            assert!(Options::parse(&json!({"modifiers":value})).is_err());
        }
        assert!(options.event(1.0,2.0,1,true).get("text").is_none());
    }

    #[test]
    fn malformed_and_unsupported_sequences_fail_without_echoing_values() {
        log("invalid-sequences");
        for args in [json!({"button":"PRIVATE-CANARY"}),json!({"button":null}),
            json!({"click_count":0}),json!({"click_count":3}),json!({"click_count":1.5}),
            json!({"click_count":"2"}),json!({"click_count":null}),
            json!({"button":"right","click_count":2}),json!({"button":"middle","click_count":2})] {
            let error = Options::parse(&args).unwrap_err().to_string();
            assert!(error.contains("BROWSER_POINTER_USAGE"));
            assert!(!error.contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn preflight_never_ignores_pointer_options_on_other_actions() {
        log("preflight-actions");
        assert!(!validate_input(&json!({"action":"click","selector":"#a"})).unwrap());
        assert!(validate_input(&json!({"action":"click","button":"left"})).unwrap());
        for action in ["press","fill","set_checked","evaluate","snapshot","scroll"] {
            for field in ["button","click_count","modifiers"] {
                let mut args = json!({"action":action});
                args[field] = Value::Null;
                assert!(validate_input(&args).is_err());
            }
        }
    }
}
