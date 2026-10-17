//! Frame-local DOM lookup and verified page-viewport input coordinates.
//!
//! The CDP connection stays attached to the page target. Same-process frame
//! nodes are resolved in their isolated world; input still uses native page
//! events. Never reuse frame-local getBoundingClientRect coordinates for them.

use super::{Cdp, document, element_call};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use serde_json::{Value, json};

const MAX_QUADS: usize = 64;
const MAX_COORDINATE: f64 = 1_000_000.0;
const QUERY: &str =
    "function(selector) { return Document.prototype.querySelector.call(document, selector); }";
const CONTAINS_HIT: &str = r"function(hit) {
    if (!this.isConnected || this.ownerDocument !== document ||
        !hit || hit.ownerDocument !== document || !hit.isConnected) return false;
    for (let node = hit, depth = 0; node && depth < 256; ++depth) {
        if (node === this) return true;
        node = node.parentNode || (node.getRootNode && node.getRootNode().host);
    }
    return false;
}";

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

async fn world(owner: &AgentCx, cdp: &mut Cdp) -> Result<u64> {
    let doc = document(owner, cdp).await?;
    let response = cdp
        .command(
            owner,
            "Page.createIsolatedWorld",
            json!({"frameId": doc.frame, "worldName": "pi-browser-tools"}),
        )
        .await?;
    let context = response["executionContextId"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| error("selected frame has no isolated world"))?;
    if document(owner, cdp).await? != doc {
        return Err(error("selected frame navigated during DOM lookup"));
    }
    Ok(context)
}

fn remote_id(response: &Value) -> Result<&str> {
    if response.get("exceptionDetails").is_some() {
        return Err(error(
            "frame DOM lookup failed; check the selector and take a new snapshot",
        ));
    }
    response["result"]["objectId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| error("frame DOM lookup returned no object handle"))
}

async fn release(owner: &AgentCx, cdp: &mut Cdp, id: &str) {
    let _ = cdp
        .command(owner, "Runtime.releaseObject", json!({"objectId": id}))
        .await;
}

/// Selector text is a JSON argument, never interpolated into executable source.
/// No-match is distinct from malformed selectors, stale worlds or protocol errors.
pub(super) async fn resolve(owner: &AgentCx, cdp: &mut Cdp, selector: &str) -> Result<Option<u64>> {
    let context = world(owner, cdp).await?;
    let response = cdp
        .command(
            owner,
            "Runtime.callFunctionOn",
            json!({
                "executionContextId": context, "functionDeclaration": QUERY,
                "arguments": [{"value": selector}], "returnByValue": false,
            }),
        )
        .await?;
    if response.get("exceptionDetails").is_none()
        && response["result"]["type"] == "object"
        && response["result"]["subtype"] == "null"
    {
        document(owner, cdp).await?;
        return Ok(None);
    }
    let id = remote_id(&response)?;
    let described = cdp
        .command(
            owner,
            "DOM.describeNode",
            json!({"objectId": id, "depth": 0}),
        )
        .await;
    release(owner, cdp, id).await;
    let backend = described?["node"]["backendNodeId"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| error("frame selector returned no backend node ID"))?;
    document(owner, cdp).await?;
    Ok(Some(backend))
}

pub(super) async fn ensure_focus(owner: &AgentCx, cdp: &mut Cdp, id: u64) -> Result<()> {
    document(owner, cdp).await?;
    if element_call(owner, cdp, id, "focused", json!({})).await? != true {
        return Err(error(
            "selected frame control lost focus; no native input was sent",
        ));
    }
    document(owner, cdp).await?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Point {
    x: f64,
    y: f64,
}

fn cross(a: Point, b: Point, c: Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn area(points: &[Point]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .map(|(a, b)| a.x * b.y - a.y * b.x)
        .sum::<f64>()
        .abs()
        / 2.0
}

fn parse_quad(value: &Value) -> Result<Vec<Point>> {
    let coordinates = value
        .as_array()
        .filter(|items| items.len() == 8)
        .ok_or_else(|| error("browser returned an invalid content quad"))?;
    let number = |value: &Value| {
        value
            .as_f64()
            .filter(|value| value.is_finite() && value.abs() <= MAX_COORDINATE)
            .ok_or_else(|| error("browser returned an invalid content-quad coordinate"))
    };
    let mut points = Vec::with_capacity(4);
    for [x, y] in coordinates.as_chunks::<2>().0 {
        points.push(Point {
            x: number(x)?,
            y: number(y)?,
        });
    }
    let turns: Vec<_> = (0..4)
        .map(|i| cross(points[i], points[(i + 1) % 4], points[(i + 2) % 4]))
        .collect();
    if turns.iter().any(|value| *value > 0.0) && turns.iter().any(|value| *value < 0.0) {
        return Err(error("browser returned a folded content quad"));
    }
    Ok(points)
}

/// Sutherland-Hodgman clipping preserves rotated quads. Clamping individual
/// vertices would invent a polygon that does not describe the visible element.
fn clip(points: Vec<Point>, axis_x: bool, boundary: f64, keep_greater: bool) -> Vec<Point> {
    let mut output = Vec::new();
    let coordinate = |point: Point| if axis_x { point.x } else { point.y };
    let inside = |point: Point| {
        if keep_greater {
            coordinate(point) >= boundary
        } else {
            coordinate(point) <= boundary
        }
    };
    let Some(mut previous) = points.last().copied() else {
        return output;
    };
    for current in points {
        if inside(previous) != inside(current) {
            let ratio =
                (boundary - coordinate(previous)) / (coordinate(current) - coordinate(previous));
            output.push(Point {
                x: previous.x + ratio * (current.x - previous.x),
                y: previous.y + ratio * (current.y - previous.y),
            });
        }
        if inside(current) {
            output.push(current);
        }
        previous = current;
    }
    output
}

fn contains(points: &[Point], point: Point) -> bool {
    let turns: Vec<_> = points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .map(|(a, b)| cross(*a, *b, point))
        .collect();
    !(turns.iter().any(|value| *value > 0.0) && turns.iter().any(|value| *value < 0.0))
}

fn candidate(value: &Value, width: f64, height: f64) -> Result<Option<(i32, i32)>> {
    let mut points = parse_quad(value)?;
    for (axis, boundary, greater) in [
        (true, 0.0, true),
        (true, width, false),
        (false, 0.0, true),
        (false, height, false),
    ] {
        points = clip(points, axis, boundary, greater);
    }
    if area(&points) <= 1.0 {
        return Ok(None);
    }
    let count = u32::try_from(points.len()).map_err(|_| error("invalid clipped quad"))?;
    let point = Point {
        x: (points.iter().map(|p| p.x).sum::<f64>() / f64::from(count)).round(),
        y: (points.iter().map(|p| p.y).sum::<f64>() / f64::from(count)).round(),
    };
    if point.x < 0.0
        || point.y < 0.0
        || point.x >= width
        || point.y >= height
        || !contains(&points, point)
    {
        return Ok(None);
    }
    // Inputs are finite and bounded to +/-1,000,000, far inside i32's range.
    #[allow(clippy::cast_possible_truncation)]
    let pixel = (point.x as i32, point.y as i32);
    Ok(Some(pixel))
}

async fn contains_hit(owner: &AgentCx, cdp: &mut Cdp, target: u64, hit: u64) -> Result<bool> {
    let context = world(owner, cdp).await?;
    let target = cdp
        .command(
            owner,
            "DOM.resolveNode",
            json!({"backendNodeId":target,"executionContextId":context}),
        )
        .await?;
    let target_id = target["object"]["objectId"]
        .as_str()
        .ok_or_else(|| error("click target has no DOM handle"))?;
    let result = async {
        let hit = cdp
            .command(
                owner,
                "DOM.resolveNode",
                json!({"backendNodeId":hit,"executionContextId":context}),
            )
            .await?;
        let hit_id = hit["object"]["objectId"]
            .as_str()
            .ok_or_else(|| error("hit-test node has no DOM handle"))?;
        let response = cdp
            .command(
                owner,
                "Runtime.callFunctionOn",
                json!({
                    "objectId":target_id,"functionDeclaration":CONTAINS_HIT,
                    "arguments":[{"objectId":hit_id}],"returnByValue":true,
                }),
            )
            .await;
        release(owner, cdp, hit_id).await;
        let response = response?;
        if response.get("exceptionDetails").is_some() {
            return Err(error("frame click hit-test failed"));
        }
        Ok(response["result"]["value"] == true)
    }
    .await;
    release(owner, cdp, target_id).await;
    result
}

/// Select a page-viewport point and verify Chromium's hit test there belongs
/// to this frame and this element (or its composed descendant). Ancestor
/// overlays, sibling frames and browser coordinate inconsistencies fail closed.
pub(super) async fn click_point(owner: &AgentCx, cdp: &mut Cdp, id: u64) -> Result<(i32, i32)> {
    let doc = document(owner, cdp).await?;
    // Retain native element visibility, enabled/inert and local occlusion checks.
    element_call(owner, cdp, id, "point", json!({})).await?;
    let response = cdp
        .command(owner, "DOM.getContentQuads", json!({"backendNodeId":id}))
        .await?;
    let quads = response["quads"]
        .as_array()
        .filter(|quads| quads.len() <= MAX_QUADS)
        .ok_or_else(|| error("browser returned too many or invalid content quads"))?;
    let metrics = cdp
        .command(owner, "Page.getLayoutMetrics", json!({}))
        .await?;
    let viewport = metrics
        .get("cssLayoutViewport")
        .or_else(|| metrics.get("layoutViewport"))
        .ok_or_else(|| error("browser returned no layout viewport"))?;
    let dimension = |key: &str| {
        viewport[key]
            .as_f64()
            .filter(|n| n.is_finite() && *n > 0.0 && *n <= MAX_COORDINATE)
            .ok_or_else(|| error("browser returned an invalid layout viewport"))
    };
    let width = dimension("clientWidth")?;
    let height = dimension("clientHeight")?;
    for quad in quads {
        let Some((x, y)) = candidate(quad, width, height)? else {
            continue;
        };
        let hit = cdp
            .command(
                owner,
                "DOM.getNodeForLocation",
                json!({
                    "x":x,"y":y,"includeUserAgentShadowDOM":false,"ignorePointerEventsNone":false,
                }),
            )
            .await?;
        if hit["frameId"].as_str() != Some(doc.frame.as_str()) {
            continue;
        }
        let hit_id = hit["backendNodeId"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| error("browser hit-test returned no node"))?;
        if contains_hit(owner, cdp, id, hit_id).await? {
            if document(owner, cdp).await? != doc {
                return Err(error("selected frame navigated before input dispatch"));
            }
            return Ok((x, y));
        }
    }
    Err(error(
        "selected frame element has no unobstructed page-viewport input point",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iframe_offset_coordinates_are_not_replaced_with_local_rect_coordinates() {
        assert_eq!(
            candidate(
                &json!([300, 200, 400, 200, 400, 240, 300, 240]),
                1000.0,
                800.0
            )
            .unwrap(),
            Some((350, 220))
        );
    }

    #[test]
    fn partially_visible_and_rotated_quads_are_clipped_before_choosing_a_point() {
        assert_eq!(
            candidate(&json!([-100, 10, 100, 10, 100, 50, -100, 50]), 800.0, 600.0).unwrap(),
            Some((50, 30))
        );
        let quad = json!([0, -100, 100, 0, 0, 100, -100, 0]);
        let (x, y) = candidate(&quad, 800.0, 600.0).unwrap().unwrap();
        assert!(x > 0 && y > 0 && x + y <= 100);
    }

    #[test]
    fn invisible_degenerate_and_subpixel_quads_do_not_generate_input() {
        for quad in [
            json!([900, 0, 950, 0, 950, 30, 900, 30]),
            json!([1, 1, 1, 1, 1, 1, 1, 1]),
            json!([0, 0, 0.5, 0, 0.5, 0.5, 0, 0.5]),
        ] {
            assert!(candidate(&quad, 800.0, 600.0).unwrap().is_none());
        }
    }

    #[test]
    fn malformed_and_folded_geometry_is_an_error_not_a_guess() {
        for quad in [
            json!([]),
            json!([0, 0, 1, 0, 1, 1, 0, "x"]),
            json!([0, 0, 1, 0, 1, 1, 0, 1e100]),
            json!([0, 0, 100, 100, 0, 100, 100, 0]),
        ] {
            assert!(candidate(&quad, 800.0, 600.0).is_err());
        }
    }

    #[test]
    fn remote_handle_failures_do_not_echo_page_exception_values() {
        let response =
            json!({"exceptionDetails":{"text":"PRIVATE-CANARY"},"result":{"objectId":"object"}});
        let error = remote_id(&response).unwrap_err().to_string();
        assert!(!error.contains("PRIVATE-CANARY"));
        assert!(remote_id(&json!({"result":{"type":"undefined"}})).is_err());
        assert_eq!(
            remote_id(&json!({"result":{"objectId":"valid"}})).unwrap(),
            "valid"
        );
    }
}
