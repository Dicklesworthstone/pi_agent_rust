//! Project renderer-local pointer candidates through the selected iframe chain.
//!
//! Every process boundary maps the child's full viewport onto the embedding
//! element's content quad (not its border box). Hit-test each parent renderer
//! at the projected point before dispatching native input on the page session.

use super::{Cdp, Scope, error, inventory};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_COORDINATE: f64 = 1_000_000.0;
const EPSILON: f64 = 1e-9;
const VIEWPORT: &str = r"({
    width: innerWidth, height: innerHeight, scrollX, scrollY,
    visibleWidth: visualViewport ? visualViewport.width : innerWidth,
    visibleHeight: visualViewport ? visualViewport.height : innerHeight,
    scale: visualViewport ? visualViewport.scale : 1,
    offsetX: visualViewport ? visualViewport.offsetLeft : 0,
    offsetY: visualViewport ? visualViewport.offsetTop : 0
})";

#[derive(Clone, Copy, Debug)]
struct Point {
    x: f64,
    y: f64,
}

fn number(value: &Value) -> Result<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite() && n.abs() <= MAX_COORDINATE)
        .ok_or_else(|| error("invalid bounded iframe pointer geometry"))
}

#[derive(Clone, Copy)]
struct Viewport {
    width: f64,
    height: f64,
    visible_width: f64,
    visible_height: f64,
    scroll_x: f64,
    scroll_y: f64,
}

impl Viewport {
    #[allow(clippy::float_cmp)] // Exact unzoomed protocol sentinels, not approximate geometry.
    fn parse(value: &Value) -> Result<Self> {
        if number(&value["scale"])? != 1.0
            || number(&value["offsetX"])? != 0.0
            || number(&value["offsetY"])? != 0.0
        {
            return Err(error(
                "frame pointer input does not support a zoomed visual viewport",
            ));
        }
        let viewport = Self {
            width: number(&value["width"])?,
            height: number(&value["height"])?,
            visible_width: number(&value["visibleWidth"])?,
            visible_height: number(&value["visibleHeight"])?,
            scroll_x: number(&value["scrollX"])?,
            scroll_y: number(&value["scrollY"])?,
        };
        if viewport.width <= 0.0
            || viewport.height <= 0.0
            || viewport.visible_width <= 0.0
            || viewport.visible_height <= 0.0
            || viewport.visible_width > viewport.width
            || viewport.visible_height > viewport.height
        {
            return Err(error("iframe pointer viewport has invalid dimensions"));
        }
        Ok(viewport)
    }

    fn admit(self, point: Point) -> Result<()> {
        if !point.x.is_finite()
            || !point.y.is_finite()
            || point.x < 0.0
            || point.y < 0.0
            || point.x >= self.visible_width
            || point.y >= self.visible_height
        {
            return Err(error(
                "projected iframe pointer is outside an ancestor viewport",
            ));
        }
        Ok(())
    }

    /// DOM hit tests take document coordinates. Check the enclosing integer
    /// grid, not just a rounded point that could approve the edge of an overlay.
    /// Native input retains the original fractional viewport coordinates.
    fn hit_grid(self, point: Point) -> Result<Vec<(i32, i32)>> {
        self.admit(point)?;
        let x = point.x + self.scroll_x;
        let y = point.y + self.scroll_y;
        if !x.is_finite()
            || !y.is_finite()
            || x.abs() > 2.0 * MAX_COORDINATE
            || y.abs() > 2.0 * MAX_COORDINATE
        {
            return Err(error(
                "projected iframe hit-test coordinate exceeds its limit",
            ));
        }
        let mut grid = Vec::with_capacity(4);
        for x in [x.floor(), x.ceil()] {
            for y in [y.floor(), y.ceil()] {
                // The bounded sums above are far inside i32's range.
                #[allow(clippy::cast_possible_truncation)]
                let pixel = (x as i32, y as i32);
                if !grid.contains(&pixel) {
                    grid.push(pixel);
                }
            }
        }
        Ok(grid)
    }
}

/// Homography from a unit rectangle onto a convex content quad. Affine
/// transforms (including rotation, skew and reflection) are the g=h=0 case.
/// No bounding-box approximation: it would misplace rotated/perspective input.
struct Projection {
    coefficients: [f64; 8],
}

impl Projection {
    fn new(value: &Value) -> Result<Self> {
        let values = value
            .as_array()
            .filter(|v| v.len() == 8)
            .ok_or_else(|| error("iframe owner has no valid content quad"))?;
        let mut points = [Point { x: 0.0, y: 0.0 }; 4];
        for (index, point) in points.iter_mut().enumerate() {
            point.x = number(&values[index * 2])?;
            point.y = number(&values[index * 2 + 1])?;
        }
        let cross =
            |a: Point, b: Point, c: Point| (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        let turns =
            [0, 1, 2, 3].map(|i| cross(points[i], points[(i + 1) % 4], points[(i + 2) % 4]));
        if !turns.iter().all(|n| *n > EPSILON) && !turns.iter().all(|n| *n < -EPSILON) {
            return Err(error(
                "iframe content quad is degenerate, folded or concave",
            ));
        }
        let [p0, p1, p2, p3] = points;
        let dx = p0.x - p1.x + p2.x - p3.x;
        let dy = p0.y - p1.y + p2.y - p3.y;
        let (g, h) = if dx.abs() <= EPSILON && dy.abs() <= EPSILON {
            (0.0, 0.0)
        } else {
            let ax = p1.x - p2.x;
            let bx = p3.x - p2.x;
            let ay = p1.y - p2.y;
            let by = p3.y - p2.y;
            let determinant = ax * by - bx * ay;
            if determinant.abs() <= EPSILON {
                return Err(error("iframe perspective projection is singular"));
            }
            (
                (dx * by - bx * dy) / determinant,
                (ax * dy - dx * ay) / determinant,
            )
        };
        // The projective denominator is linear over the source rectangle.
        // Positive corners rule out a pole anywhere inside it.
        if [1.0, 1.0 + g, 1.0 + h, 1.0 + g + h]
            .iter()
            .any(|w| !w.is_finite() || *w <= EPSILON)
        {
            return Err(error("iframe perspective projection crosses a horizon"));
        }
        Ok(Self {
            coefficients: [
                p1.x - p0.x + g * p1.x,
                p3.x - p0.x + h * p3.x,
                p0.x,
                p1.y - p0.y + g * p1.y,
                p3.y - p0.y + h * p3.y,
                p0.y,
                g,
                h,
            ],
        })
    }

    #[allow(clippy::many_single_char_names)] // Standard homography coefficients and homogeneous coordinates.
    fn map(&self, point: Point, viewport: Viewport) -> Result<Point> {
        viewport.admit(point)?;
        // Full viewport dimensions include scrollbars. Dividing by clientWidth
        // would stretch every point when a child starts scrolling vertically.
        let u = point.x / viewport.width;
        let v = point.y / viewport.height;
        let [a, b, c, d, e, f, g, h] = self.coefficients;
        let w = g * u + h * v + 1.0;
        let result = Point {
            x: (a * u + b * v + c) / w,
            y: (d * u + e * v + f) / w,
        };
        if w <= EPSILON
            || !result.x.is_finite()
            || !result.y.is_finite()
            || result.x.abs() > MAX_COORDINATE
            || result.y.abs() > MAX_COORDINATE
        {
            return Err(error(
                "iframe pointer projection exceeds its geometry limit",
            ));
        }
        Ok(result)
    }
}

async fn viewport(
    owner: &AgentCx,
    cdp: &mut Cdp,
    scope: &Scope,
    session: &str,
) -> Result<Viewport> {
    let frame = scope
        .ancestry
        .iter()
        .find(|frame| frame.session == session)
        .ok_or_else(|| error("pointer renderer is not in the selected ancestry"))?;
    let world = cdp
        .frame_command(
            owner,
            session,
            "Page.createIsolatedWorld",
            json!({"frameId": frame.id, "worldName": "pi-browser-pointer"}),
        )
        .await?;
    let context = world["executionContextId"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| error("pointer renderer has no isolated viewport world"))?;
    let response = cdp
        .frame_command(
            owner,
            session,
            "Runtime.evaluate",
            json!({
                "expression": VIEWPORT, "contextId": context, "returnByValue": true,
                "allowUnsafeEvalBlockedByCSP": false,
            }),
        )
        .await?;
    if response.get("exceptionDetails").is_some() {
        return Err(error("could not inspect the pointer renderer viewport"));
    }
    Viewport::parse(&response["result"]["value"])
}

/// This is a geometry projection, not a second frame authorization. The active
/// CDP scope is checked before and after the inventory and every original
/// ancestor/renderer must still match. No pointer is dispatched here.
// `Link` has no `id` field: a link's `target` is the frame id it is matched against.
#[allow(clippy::too_many_lines, clippy::suspicious_operation_groupings)]
pub(in crate::browser) async fn project(
    owner: &AgentCx,
    cdp: &mut Cdp,
    frame: &str,
    x: i32,
    y: i32,
) -> Result<(f64, f64)> {
    let original = cdp.frame_document(owner).await?;
    if original["id"].as_str() != Some(frame) {
        return Err(error("pointer candidate belongs to another frame"));
    }
    let tree = inventory(owner, cdp).await?;
    let scope = Scope::pin(&tree, frame, &cdp.frame_targets)?;
    scope.check_current(owner, cdp).await?;
    cdp.frame_document(owner).await?;
    let mut point = Point {
        x: f64::from(x),
        y: f64::from(y),
    };
    if !scope.is_remote() {
        return Ok((point.x, point.y));
    }
    let mut viewports = BTreeMap::new();
    let mut session = scope.selected()?.session.as_str();
    for link in scope.routes.iter().rev() {
        if link.session != session {
            return Err(error("pointer projection is missing a renderer boundary"));
        }
        scope.check_current(owner, cdp).await?;
        for key in [link.session.as_str(), link.parent.as_str()] {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                viewports.entry(key.to_owned())
            {
                entry.insert(viewport(owner, cdp, &scope, key).await?);
            }
        }
        let child = scope
            .ancestry
            .iter()
            .find(|item| item.id == link.target && item.session == link.session)
            .ok_or_else(|| error("pointer iframe is not in the selected ancestry"))?;
        let parent = child
            .parent
            .as_deref()
            .ok_or_else(|| error("pointer iframe has no parent"))?;
        let owner_node = cdp
            .frame_command(
                owner,
                &link.parent,
                "DOM.getFrameOwner",
                json!({"frameId":link.target}),
            )
            .await?;
        let backend = owner_node["backendNodeId"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(|| error("pointer iframe has no embedding node"))?;
        let model = cdp
            .frame_command(
                owner,
                &link.parent,
                "DOM.getBoxModel",
                json!({"backendNodeId":backend}),
            )
            .await?;
        point =
            Projection::new(&model["model"]["content"])?.map(point, viewports[&link.session])?;
        for (document_x, document_y) in viewports[&link.parent].hit_grid(point)? {
            let hit = cdp
                .frame_command(
                    owner,
                    &link.parent,
                    "DOM.getNodeForLocation",
                    json!({
                        "x":document_x, "y":document_y,
                        "includeUserAgentShadowDOM":false, "ignorePointerEventsNone":false,
                    }),
                )
                .await?;
            if hit["backendNodeId"].as_u64() != Some(backend)
                || hit["frameId"].as_str() != Some(parent)
            {
                return Err(error(
                    "iframe pointer is occluded by an ancestor or another frame",
                ));
            }
        }
        session = &link.parent;
    }
    if session != cdp.page_session_id()? {
        return Err(error("pointer projection did not reach the selected page"));
    }
    scope.check_current(owner, cdp).await?;
    cdp.frame_document(owner).await?;
    Ok((point.x, point.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(width: f64, height: f64) -> Viewport {
        Viewport {
            width,
            height,
            visible_width: width,
            visible_height: height,
            scroll_x: 0.0,
            scroll_y: 0.0,
        }
    }

    fn assert_point(point: Point, x: f64, y: f64) {
        assert!((point.x - x).abs() < 1e-6, "{} != {x}", point.x);
        assert!((point.y - y).abs() < 1e-6, "{} != {y}", point.y);
    }

    #[test]
    fn content_quad_accounts_for_borders_padding_and_scrollbars() {
        let projection = Projection::new(&json!([227, 137, 647, 137, 647, 437, 227, 437])).unwrap();
        let mut viewport = view(420.0, 300.0);
        viewport.visible_width = 405.0;
        assert_point(
            projection
                .map(Point { x: 105.0, y: 120.0 }, viewport)
                .unwrap(),
            332.0,
            257.0,
        );
    }

    #[test]
    fn nested_renderer_transforms_compose_without_border_box_offsets() {
        let inner = Projection::new(&json!([100, 50, 500, 50, 500, 350, 100, 350])).unwrap();
        let outer = Projection::new(&json!([50, 30, 650, 30, 650, 480, 50, 480])).unwrap();
        let point = inner
            .map(Point { x: 105.0, y: 120.0 }, view(200.0, 150.0))
            .unwrap();
        assert_point(point, 310.0, 290.0);
        assert_point(outer.map(point, view(600.0, 450.0)).unwrap(), 360.0, 320.0);
    }

    #[test]
    fn rotation_reflection_and_perspective_use_quad_geometry() {
        let point = Point { x: 50.0, y: 25.0 };
        let viewport = view(200.0, 100.0);
        let rotated = Projection::new(&json!([100, 100, 100, 300, 0, 300, 0, 100])).unwrap();
        assert_point(rotated.map(point, viewport).unwrap(), 75.0, 150.0);
        let reflected = Projection::new(&json!([200, 0, 0, 0, 0, 100, 200, 100])).unwrap();
        assert_point(reflected.map(point, viewport).unwrap(), 150.0, 25.0);
        let perspective = Projection::new(&json!([0, 0, 200, 0, 150, 100, 50, 100])).unwrap();
        assert_point(
            perspective
                .map(Point { x: 100.0, y: 50.0 }, viewport)
                .unwrap(),
            100.0,
            200.0 / 3.0,
        );
    }

    #[test]
    fn malformed_folded_concave_and_degenerate_quads_never_produce_input() {
        for quad in [
            json!([]),
            json!([0, 0, 100, 0, 100, 100, 0, "PRIVATE-CANARY"]),
            json!([0, 0, 100, 0, 100, 100, 0, 1e100]),
            json!([0, 0, 100, 100, 0, 100, 100, 0]),
            json!([0, 0, 0, 0, 0, 0, 0, 0]),
            json!([0, 0, 100, 0, 20, 20, 0, 100]),
        ] {
            let error = Projection::new(&quad)
                .err()
                .expect("invalid quad")
                .to_string();
            assert!(!error.contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn document_hit_grid_preserves_signed_scroll_and_fractional_native_points() {
        let mut viewport = view(600.0, 450.0);
        viewport.scroll_x = -20.0;
        viewport.scroll_y = 800.0;
        let point = Point {
            x: 105.25,
            y: 120.75,
        };
        assert_eq!(
            viewport.hit_grid(point).unwrap(),
            [(85, 920), (85, 921), (86, 920), (86, 921)],
        );
        assert_point(point, 105.25, 120.75);
        assert_eq!(
            viewport.hit_grid(Point { x: 105.0, y: 120.0 }).unwrap(),
            [(85, 920)],
        );
        assert!(viewport.hit_grid(Point { x: 600.0, y: 0.0 }).is_err());
        assert!(
            viewport
                .hit_grid(Point {
                    x: f64::NAN,
                    y: 0.0
                })
                .is_err()
        );
    }

    #[test]
    fn viewport_rejects_missing_malformed_zoomed_or_inconsistent_geometry() {
        let good = json!({
            "width":420, "height":300, "visibleWidth":405, "visibleHeight":300,
            "scrollX":0, "scrollY":800, "scale":1, "offsetX":0, "offsetY":0,
        });
        assert!(Viewport::parse(&good).is_ok());
        for (key, value) in [
            ("width", json!(0)),
            ("visibleWidth", json!(421)),
            ("scale", json!(2)),
            ("offsetX", json!(1)),
            ("scrollY", json!(1e100)),
            ("height", Value::Null),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(Viewport::parse(&bad).is_err(), "{key}");
        }
        assert!(Viewport::parse(&json!({})).is_err());
    }
}
