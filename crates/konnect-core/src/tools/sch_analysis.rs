//! `sch_analysis` toolset — net connectivity, pin queries, trace paths, overlap/orphan detection.
//!
//! All operations are read-only S-expression analysis.
//! Net graph uses union-find (O(W+L+P)), matching net_analysis.py.

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::{get_path, opt_f64, require_f64, require_str, ToolContext, ToolDef};
use konnect_schematic_editor as cse;
use konnect_sexp::{
    geometry::{point_on_segment, points_coincident},
    schematic::{
        extract_junctions, extract_labels, extract_lib_pins_for_unit, extract_symbol_instances,
        extract_wires, find_lib_symbol, pin_endpoint, read_schematic, Wire,
    },
};
use serde_json::json;
use std::collections::{HashMap, HashSet};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "list_schematic_wires",
            "List all wire segments in a schematic with start/end coordinates and UUIDs.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_list_wires(args, ctx).await }
        ),
        tool!(
            "list_schematic_nets",
            "List all distinct net names derived from net labels, global labels, and power symbols.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_list_nets(args, ctx).await }
        ),
        tool!(
            "list_schematic_labels",
            "List all label instances (net_label, global_label, hierarchical_label) \
             with their positions, net names, and types.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_list_labels(args, ctx).await }
        ),
        tool!(
            "get_net_connections",
            "Get all pins and labels connected to a named net.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string", "description": "Net name to query" }
                },
                "required": ["schematic", "net"] }),
            |args, ctx| async move { handle_get_net_connections(args, ctx).await }
        ),
        tool!(
            "get_net_connectivity",
            "Build the full connectivity graph for a net using union-find. \
             Returns all wire segments, labels, and T-junction locations.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" }
                },
                "required": ["schematic", "net"] }),
            |args, ctx| async move { handle_get_net_connectivity(args, ctx).await }
        ),
        tool!(
            "get_pin_connections",
            "Get the net connected to a specific pin on a component by tracing wires from the pin endpoint.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "pin_number": { "type": "string" }
                },
                "required": ["schematic", "reference", "pin_number"] }),
            |args, ctx| async move { handle_get_pin_connections(args, ctx).await }
        ),
        tool!(
            "get_pin_net_name",
            "Return just the net name for a specific pin on a component.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" },
                    "pin_number": { "type": "string" }
                },
                "required": ["schematic", "reference", "pin_number"] }),
            |args, ctx| async move { handle_get_pin_connections(args, ctx).await }
        ),
        tool!(
            "get_component_nets",
            "Get all nets connected to every pin of a component.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "reference": { "type": "string" }
                },
                "required": ["schematic", "reference"] }),
            |args, ctx| async move { handle_get_component_nets(args, ctx).await }
        ),
        tool!(
            "get_net_components",
            "Get all components (and their pins) connected to a named net.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "net": { "type": "string" }
                },
                "required": ["schematic", "net"] }),
            |args, ctx| async move { handle_get_net_components(args, ctx).await }
        ),
        tool!(
            "trace_from_point",
            "Trace connectivity from any (X,Y) point — returns what is at that point and the net it belongs to.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "tolerance": { "type": "number", "default": 0.05 }
                },
                "required": ["schematic", "x", "y"] }),
            |args, ctx| async move { handle_trace_from_point(args, ctx).await }
        ),
        tool!(
            "find_orphan_items",
            "Find dangling wire ends, floating labels, and unconnected pin endpoints. \
             Pins, sheet pins, junctions, and no-connect flags all count as connections.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "tolerance": {
                        "type": "number", "exclusiveMinimum": 0, "default": 0.05
                    }
                },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_find_orphan_items(args, ctx).await }
        ),
        tool!(
            "find_shorted_nets",
            "Detect accidentally merged nets — pairs of distinct net names sharing a wire path.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_find_shorted_nets(args, ctx).await }
        ),
        tool!(
            "find_single_pin_nets",
            "Find nets with only one label/connection — often indicates a missing counterpart.",
            json!({ "type": "object",
                "properties": { "schematic": { "type": "string" } },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_find_single_pin_nets(args, ctx).await }
        ),
        tool!(
            "get_connected_items",
            "Get all wires, labels, and components connected to a given component reference \
             by tracing net connectivity from each of its pins.",
            json!({
                "type": "object",
                "properties": {
                    "schematic": { "type": "string", "description": "Path to .kicad_sch file" },
                    "reference": { "type": "string", "description": "Component reference designator (e.g. 'R1')" }
                },
                "required": ["schematic", "reference"]
            }),
            |args, ctx| async move { handle_get_connected_items(args, ctx).await }
        ),
        tool!(
            "check_schematic_overlaps",
            "Find overlapping symbols or labels that may indicate placement errors.",
            json!({ "type": "object",
                "properties": {
                    "schematic": { "type": "string" },
                    "tolerance": { "type": "number", "default": 0.5 }
                },
                "required": ["schematic"] }),
            |args, ctx| async move { handle_check_overlaps(args, ctx).await }
        ),
    ]
}

// ─── Union-Find net graph ─────────────────────────────────────────────────────

pub(crate) fn pt_key(x: f64, y: f64) -> (i64, i64) {
    ((x * 1000.0).round() as i64, (y * 1000.0).round() as i64)
}

pub(crate) struct NetGraph {
    pub(crate) point_nets: HashMap<(i64, i64), String>,
    pub(crate) parent: HashMap<(i64, i64), (i64, i64)>,
}

impl NetGraph {
    pub(crate) fn new() -> Self {
        NetGraph {
            point_nets: HashMap::new(),
            parent: HashMap::new(),
        }
    }

    pub(crate) fn ensure(&mut self, k: (i64, i64)) {
        self.parent.entry(k).or_insert(k);
    }

    pub(crate) fn find(&mut self, k: (i64, i64)) -> (i64, i64) {
        self.ensure(k);
        let p = self.parent[&k];
        if p == k {
            return k;
        }
        let root = self.find(p);
        self.parent.insert(k, root);
        root
    }

    pub(crate) fn union(&mut self, a: (i64, i64), b: (i64, i64)) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent.insert(rb, ra);
        }
    }

    pub(crate) fn add_wire(&mut self, w: &Wire) {
        let a = pt_key(w.x1, w.y1);
        let b = pt_key(w.x2, w.y2);
        self.ensure(a);
        self.ensure(b);
        self.union(a, b);
    }

    pub(crate) fn add_label(&mut self, x: f64, y: f64, net: &str) {
        let k = pt_key(x, y);
        self.ensure(k);
        self.point_nets.insert(k, net.to_string());
    }

    pub(crate) fn net_at(&mut self, x: f64, y: f64) -> Option<String> {
        let k = pt_key(x, y);
        self.ensure(k);
        let root = self.find(k);
        let labels: Vec<_> = self.point_nets.clone().into_iter().collect();
        for (lk, net) in labels {
            if self.find(lk) == root {
                return Some(net);
            }
        }
        None
    }

    pub(crate) fn points_on_net(&mut self, net: &str) -> Vec<(i64, i64)> {
        // Collect keys first to avoid simultaneous borrow of point_nets and self.find()
        let net_keys: Vec<(i64, i64)> = self
            .point_nets
            .iter()
            .filter(|(_, n)| n.as_str() == net)
            .map(|(k, _)| *k)
            .collect();
        let net_roots: HashSet<(i64, i64)> = net_keys.iter().map(|k| self.find(*k)).collect();
        let all_keys: Vec<(i64, i64)> = self.parent.keys().cloned().collect();
        all_keys
            .into_iter()
            .filter(|k| net_roots.contains(&self.find(*k)))
            .collect()
    }
}

pub(crate) fn build_net_graph(
    wires: &[Wire],
    labels: &[konnect_sexp::schematic::Label],
    junctions: &[(f64, f64)],
) -> NetGraph {
    let mut g = NetGraph::new();
    for w in wires {
        g.add_wire(w);
    }
    // Labels and junction dots connect anywhere along a wire, not only at
    // endpoints — union each such point with the segment it sits on.
    // ponytail: O(P×W) scan; fine at schematic scale, index wires if it hurts.
    let attach = |g: &mut NetGraph, x: f64, y: f64| {
        for w in wires {
            if point_on_segment(x, y, w.x1, w.y1, w.x2, w.y2, 0.01) {
                g.union(pt_key(x, y), pt_key(w.x1, w.y1));
            }
        }
    };
    for l in labels {
        g.add_label(l.x, l.y, &l.net);
        attach(&mut g, l.x, l.y);
    }
    for &(jx, jy) in junctions {
        g.ensure(pt_key(jx, jy));
        attach(&mut g, jx, jy);
    }
    g
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_list_wires(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;
    let items: Vec<serde_json::Value> = sch.wires.iter()
        .map(|w| json!({ "x1": w.start.0, "y1": w.start.1, "x2": w.end.0, "y2": w.end.1, "uuid": w.uuid }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "wires": items }),
    ))
}

async fn handle_list_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;
    let mut nets: Vec<String> = sch
        .labels
        .iter()
        .map(|l| l.text.clone())
        .chain(sch.global_labels.iter().map(|l| l.text.clone()))
        .chain(sch.hierarchical_labels.iter().map(|l| l.text.clone()))
        .collect();
    nets.sort();
    nets.dedup();
    Ok(CallToolResult::json(
        &json!({ "count": nets.len(), "nets": nets }),
    ))
}

async fn handle_list_labels(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;
    let mut items: Vec<serde_json::Value> = Vec::new();
    for l in sch.labels.iter() {
        items.push(json!({ "net": l.text, "type": "NetLabel", "x": l.at.x, "y": l.at.y, "rotation": l.at.rotation.unwrap_or(0.0) }));
    }
    for g in sch.global_labels.iter() {
        items.push(json!({ "net": g.text, "type": "GlobalLabel", "x": g.at.x, "y": g.at.y, "rotation": g.at.rotation.unwrap_or(0.0) }));
    }
    for h in sch.hierarchical_labels.iter() {
        items.push(json!({ "net": h.text, "type": "HierarchicalLabel", "x": h.at.x, "y": h.at.y, "rotation": h.at.rotation.unwrap_or(0.0) }));
    }
    Ok(CallToolResult::json(
        &json!({ "count": items.len(), "labels": items }),
    ))
}

async fn handle_get_net_connections(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let sch = cse::Schematic::load(&sch_path)?;
    let wires = super::sch_bridge::all_wires_as_sexp(&sch);
    let labels = super::sch_bridge::all_labels_as_sexp(&sch);
    let matching: Vec<_> = labels
        .iter()
        .filter(|l| l.net == net)
        .map(|l| json!({ "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
        .collect();
    let mut g = build_net_graph(&wires, &labels, &super::sch_bridge::all_junctions(&sch));
    let pts = g.points_on_net(&net).len();
    Ok(CallToolResult::json(
        &json!({ "net": net, "label_count": matching.len(), "labels": matching, "connected_points": pts }),
    ))
}

async fn handle_get_net_connectivity(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let sch = cse::Schematic::load(&sch_path)?;
    let wires = super::sch_bridge::all_wires_as_sexp(&sch);
    let labels = super::sch_bridge::all_labels_as_sexp(&sch);
    let mut g = build_net_graph(&wires, &labels, &super::sch_bridge::all_junctions(&sch));
    let net_pts: HashSet<(i64, i64)> = g.points_on_net(&net).into_iter().collect();
    let net_wires: Vec<_> = wires
        .iter()
        .filter(|w| net_pts.contains(&pt_key(w.x1, w.y1)) || net_pts.contains(&pt_key(w.x2, w.y2)))
        .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2 }))
        .collect();
    let net_labels: Vec<_> = labels
        .iter()
        .filter(|l| l.net == net)
        .map(|l| json!({ "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
        .collect();
    let net_wire_objs: Vec<Wire> = wires
        .iter()
        .filter(|w| net_pts.contains(&pt_key(w.x1, w.y1)) || net_pts.contains(&pt_key(w.x2, w.y2)))
        .cloned()
        .collect();
    let t_junctions = konnect_sexp::schematic::find_t_junctions(&net_wire_objs, 0.01);
    Ok(CallToolResult::json(&json!({
        "net": net,
        "wires": net_wires,
        "labels": net_labels,
        "t_junctions": t_junctions.iter().map(|(x,y)| json!({"x": x, "y": y})).collect::<Vec<_>>()
    })))
}

async fn handle_get_pin_connections(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let pin_number = match require_str(args, "pin_number") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let (pin, transform) =
        match super::sch_wiring::resolve_placed_pin(&instances, &lib_syms, &reference, &pin_number)
        {
            Ok(placed) => placed,
            Err(error) => return Ok(CallToolResult::error(format!("{error:#}"))),
        };
    let (px, py) = pin_endpoint(&pin, transform);
    let mut g = build_net_graph(&wires, &labels, &extract_junctions(&tree));
    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pin": pin_number, "pin_x": px, "pin_y": py, "net": g.net_at(px, py) }),
    ))
}

async fn handle_get_component_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let placed: Vec<_> = instances
        .iter()
        .filter(|instance| instance.reference == reference)
        .collect();
    if placed.is_empty() {
        return Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        )));
    }
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut g = build_net_graph(&wires, &labels, &extract_junctions(&tree));
    let mut pins: Vec<serde_json::Value> = Vec::new();
    for instance in placed {
        let Some(symbol) = find_lib_symbol(&lib_syms, instance) else {
            continue;
        };
        let transform = instance.pin_transform();
        for pin in extract_lib_pins_for_unit(symbol, instance.unit) {
            let (px, py) = pin_endpoint(&pin, transform);
            pins.push(json!({
                "pin": pin.number,
                "name": pin.name,
                "unit": instance.unit,
                "x": px,
                "y": py,
                "net": g.net_at(px, py)
            }));
        }
    }
    Ok(CallToolResult::json(
        &json!({ "reference": reference, "pins": pins }),
    ))
}

async fn handle_get_net_components(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let net = match require_str(args, "net") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };
    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();
    let mut g = build_net_graph(&wires, &labels, &extract_junctions(&tree));
    let net_pts: HashSet<(i64, i64)> = g.points_on_net(&net).into_iter().collect();
    let result: Vec<serde_json::Value> = instances
        .iter()
        .filter_map(|inst| {
            let ls = find_lib_symbol(&lib_syms, inst)?;
            let t = inst.pin_transform();
            let connected: Vec<_> = extract_lib_pins_for_unit(ls, inst.unit)
                .iter()
                .filter_map(|p| {
                    let (px, py) = konnect_sexp::schematic::pin_endpoint(p, t);
                    if net_pts.contains(&pt_key(px, py)) {
                        Some(json!({ "pin": p.number, "name": p.name, "unit": inst.unit }))
                    } else {
                        None
                    }
                })
                .collect();
            if connected.is_empty() {
                None
            } else {
                Some(json!({
                    "reference": inst.reference,
                    "value": inst.value,
                    "unit": inst.unit,
                    "pins": connected
                }))
            }
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "net": net, "components": result }),
    ))
}

async fn handle_trace_from_point(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let x = match require_f64(args, "x") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let y = match require_f64(args, "y") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let tol = opt_f64(args, "tolerance").unwrap_or(0.05);
    let sch = cse::Schematic::load(&sch_path)?;
    let wires = super::sch_bridge::all_wires_as_sexp(&sch);
    let labels = super::sch_bridge::all_labels_as_sexp(&sch);
    let mut g = build_net_graph(&wires, &labels, &super::sch_bridge::all_junctions(&sch));
    let on_wire: Vec<_> = wires
        .iter()
        .filter(|w| {
            points_coincident(x, y, w.x1, w.y1, tol)
                || points_coincident(x, y, w.x2, w.y2, tol)
                || point_on_segment(x, y, w.x1, w.y1, w.x2, w.y2, tol)
        })
        .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2 }))
        .collect();
    let at_label: Vec<_> = labels
        .iter()
        .filter(|l| points_coincident(x, y, l.x, l.y, tol))
        .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind) }))
        .collect();
    Ok(CallToolResult::json(
        &json!({ "x": x, "y": y, "net": g.net_at(x, y), "wires_here": on_wire, "labels_here": at_label }),
    ))
}

/// Points bucketed at the coincidence tolerance, so a lookup probes nine cells
/// instead of scanning every point. `points_coincident` compares an L∞ box of
/// side `tol`, which the 3×3 neighbourhood covers exactly.
struct PointIndex {
    tol: f64,
    buckets: HashMap<(i64, i64), Vec<(f64, f64)>>,
}

impl PointIndex {
    fn build(points: impl IntoIterator<Item = (f64, f64)>, tol: f64) -> Self {
        let mut index = PointIndex {
            tol,
            buckets: HashMap::new(),
        };
        for (x, y) in points {
            let key = index.cell(x, y);
            index.buckets.entry(key).or_default().push((x, y));
        }
        index
    }

    fn cell(&self, x: f64, y: f64) -> (i64, i64) {
        ((x / self.tol).floor() as i64, (y / self.tol).floor() as i64)
    }

    /// How many indexed points coincide with `(x, y)`.
    fn count_at(&self, x: f64, y: f64) -> usize {
        let (cx, cy) = self.cell(x, y);
        let mut found = 0;
        for dx in -1..=1 {
            for dy in -1..=1 {
                let Some(bucket) = self.buckets.get(&(cx + dx, cy + dy)) else {
                    continue;
                };
                found += bucket
                    .iter()
                    .filter(|(px, py)| points_coincident(x, y, *px, *py, self.tol))
                    .count();
            }
        }
        found
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        self.count_at(x, y) > 0
    }
}

/// Wires bucketed by the coordinate they hold constant: a horizontal wire can
/// only be met in its own row, a vertical one in its own column. Mirrors
/// `point_on_segment`, which answers `false` for anything diagonal.
struct WireIndex<'a> {
    tol: f64,
    rows: HashMap<i64, Vec<&'a Wire>>,
    columns: HashMap<i64, Vec<&'a Wire>>,
}

impl<'a> WireIndex<'a> {
    fn build(wires: &'a [Wire], tol: f64) -> Self {
        let mut index = WireIndex {
            tol,
            rows: HashMap::new(),
            columns: HashMap::new(),
        };
        for wire in wires {
            if (wire.x1 - wire.x2).abs() < tol {
                index
                    .columns
                    .entry(bucket(wire.x1, tol))
                    .or_default()
                    .push(wire);
            } else if (wire.y1 - wire.y2).abs() < tol {
                index
                    .rows
                    .entry(bucket(wire.y1, tol))
                    .or_default()
                    .push(wire);
            }
        }
        index
    }

    /// Every wire that could pass through `(x, y)`.
    fn candidates(&self, x: f64, y: f64) -> impl Iterator<Item = &&'a Wire> {
        let cell_x = bucket(x, self.tol);
        let cell_y = bucket(y, self.tol);
        (-1..=1).flat_map(move |delta| {
            let column = self.columns.get(&(cell_x + delta)).into_iter().flatten();
            let row = self.rows.get(&(cell_y + delta)).into_iter().flatten();
            column.chain(row)
        })
    }

    /// Lies anywhere on a wire, endpoints included.
    fn covers(&self, x: f64, y: f64) -> bool {
        self.candidates(x, y)
            .any(|wire| point_on_segment(x, y, wire.x1, wire.y1, wire.x2, wire.y2, self.tol))
    }

    /// Lies on the interior of a wire — a T-junction, which KiCAD connects
    /// without splitting the crossed wire.
    fn covers_interior(&self, x: f64, y: f64) -> bool {
        self.candidates(x, y).any(|wire| {
            point_on_segment(x, y, wire.x1, wire.y1, wire.x2, wire.y2, self.tol)
                && !points_coincident(x, y, wire.x1, wire.y1, self.tol)
                && !points_coincident(x, y, wire.x2, wire.y2, self.tol)
        })
    }
}

fn bucket(value: f64, tolerance: f64) -> i64 {
    (value / tolerance).floor() as i64
}

async fn handle_find_orphan_items(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tolerance = opt_f64(args, "tolerance").unwrap_or(0.05);
    if !tolerance.is_finite() || tolerance <= 0.0 {
        return Ok(CallToolResult::error(
            "Invalid argument 'tolerance': must be finite and positive",
        ));
    }

    let (_, tree) = read_schematic(&sch_path)?;
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);

    // Unit-aware, so a multi-unit symbol does not contribute another unit's
    // pins as phantom connection points (#35).
    let placed = crate::tools::placed_pins_by_reference(&tree);
    let pins: Vec<(&str, &konnect_sexp::schematic::LibPin, (f64, f64))> = placed
        .iter()
        .flat_map(|(reference, pins)| {
            pins.iter().map(move |(pin, transform)| {
                (
                    reference.as_str(),
                    pin,
                    konnect_sexp::schematic::pin_endpoint(pin, *transform),
                )
            })
        })
        .collect();

    let on_wire = WireIndex::build(&wires, tolerance);
    let wire_ends = PointIndex::build(
        wires
            .iter()
            .flat_map(|wire| [(wire.x1, wire.y1), (wire.x2, wire.y2)]),
        tolerance,
    );
    let label_points = PointIndex::build(labels.iter().map(|label| (label.x, label.y)), tolerance);
    let pin_points = PointIndex::build(pins.iter().map(|(_, _, at)| *at), tolerance);
    let junctions = PointIndex::build(extract_junctions(&tree), tolerance);
    let no_connects = PointIndex::build(
        konnect_sexp::schematic::extract_no_connects(&tree),
        tolerance,
    );
    let sheet_pins = PointIndex::build(
        konnect_sexp::schematic::extract_sheet_pins(&tree),
        tolerance,
    );

    let mut all: Vec<serde_json::Value> = Vec::new();

    // A wire end is dangling only when nothing terminates it. Ending on a
    // component or hierarchical sheet pin is the normal case.
    for wire in &wires {
        for (x, y) in [(wire.x1, wire.y1), (wire.x2, wire.y2)] {
            let connected = pin_points.contains(x, y)
                || label_points.contains(x, y)
                || sheet_pins.contains(x, y)
                || junctions.contains(x, y)
                || no_connects.contains(x, y)
                // This end is itself indexed; a second hit is another wire.
                || wire_ends.count_at(x, y) >= 2
                || on_wire.covers_interior(x, y);
            if !connected {
                all.push(json!({
                    "type": "dangling_wire_end",
                    "x": x,
                    "y": y,
                    "wire_uuid": wire.uuid
                }));
            }
        }
    }

    // Labels connect anywhere along a wire, not only at its endpoint, or
    // directly on a bare symbol pin.
    for label in &labels {
        if !on_wire.covers(label.x, label.y) && !pin_points.contains(label.x, label.y) {
            all.push(json!({
                "type": "floating_label",
                "net": label.net,
                "x": label.x,
                "y": label.y
            }));
        }
    }

    // Report the unconnected pins promised by the tool description. A pin
    // sitting mid-wire connects only through a junction dot (#104).
    for (reference, pin, (x, y)) in &pins {
        let (x, y) = (*x, *y);
        if pin.electrical_type == "no_connect" || no_connects.contains(x, y) {
            continue;
        }
        let connected = wire_ends.contains(x, y)
            || label_points.contains(x, y)
            // This pin is itself indexed; a second hit is a stacked pin.
            || pin_points.count_at(x, y) >= 2
            || (junctions.contains(x, y) && on_wire.covers(x, y));
        if !connected {
            all.push(json!({
                "type": "unconnected_pin",
                "reference": reference,
                "pin": pin.number,
                "pin_name": pin.name,
                "x": x,
                "y": y
            }));
        }
    }

    Ok(CallToolResult::json(&json!({
        "orphan_count": all.len(),
        "orphans": all,
        "tolerance": tolerance
    })))
}

async fn handle_find_shorted_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;
    let wires = super::sch_bridge::all_wires_as_sexp(&sch);
    let labels = super::sch_bridge::all_labels_as_sexp(&sch);
    let mut g = build_net_graph(&wires, &labels, &super::sch_bridge::all_junctions(&sch));
    let mut root_nets: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    for l in &labels {
        let root = g.find(pt_key(l.x, l.y));
        root_nets.entry(root).or_default().push(l.net.clone());
    }
    let shorts: Vec<serde_json::Value> = root_nets
        .into_values()
        .filter_map(|mut nets| {
            nets.sort();
            nets.dedup();
            if nets.len() > 1 {
                Some(json!({ "shorted_nets": nets }))
            } else {
                None
            }
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "short_count": shorts.len(), "shorts": shorts }),
    ))
}

async fn handle_find_single_pin_nets(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let sch = cse::Schematic::load(&sch_path)?;
    let labels = super::sch_bridge::all_labels_as_sexp(&sch);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for l in &labels {
        *counts.entry(l.net.clone()).or_insert(0) += 1;
    }
    let singles: Vec<serde_json::Value> = counts
        .iter()
        .filter(|(_, &c)| c == 1)
        .map(|(net, _)| {
            let l = labels.iter().find(|l| &l.net == net).unwrap();
            json!({ "net": net, "x": l.x, "y": l.y, "type": format!("{:?}", l.kind) })
        })
        .collect();
    Ok(CallToolResult::json(
        &json!({ "single_pin_net_count": singles.len(), "nets": singles }),
    ))
}

async fn handle_get_connected_items(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let reference = match require_str(args, "reference") {
        Ok(v) => v.to_string(),
        Err(e) => return Ok(e),
    };

    let (_, tree) = read_schematic(&sch_path)?;
    let instances = extract_symbol_instances(&tree);
    let wires = extract_wires(&tree);
    let labels = extract_labels(&tree);
    let lib_syms = tree
        .find("lib_symbols")
        .map(|n| n.find_all("symbol"))
        .unwrap_or_default();

    let placed: Vec<_> = instances
        .iter()
        .filter(|instance| instance.reference == reference)
        .collect();
    if placed.is_empty() {
        return Ok(CallToolResult::error(format!(
            "Component '{}' not found",
            reference
        )));
    }
    let mut g = build_net_graph(&wires, &labels, &extract_junctions(&tree));

    // Get nets for every actually placed unit of the component.
    let mut connected_nets: HashSet<String> = HashSet::new();
    for instance in placed {
        let Some(symbol) = find_lib_symbol(&lib_syms, instance) else {
            continue;
        };
        let transform = instance.pin_transform();
        for pin in extract_lib_pins_for_unit(symbol, instance.unit) {
            let (px, py) = pin_endpoint(&pin, transform);
            if let Some(net) = g.net_at(px, py) {
                connected_nets.insert(net);
            }
        }
    }

    // Find all wires, labels, and components on those nets
    let mut all_net_pts: HashSet<(i64, i64)> = HashSet::new();
    for net in &connected_nets {
        for pt in g.points_on_net(net) {
            all_net_pts.insert(pt);
        }
    }

    let connected_wires: Vec<serde_json::Value> = wires
        .iter()
        .filter(|w| {
            all_net_pts.contains(&pt_key(w.x1, w.y1)) || all_net_pts.contains(&pt_key(w.x2, w.y2))
        })
        .map(|w| json!({ "x1": w.x1, "y1": w.y1, "x2": w.x2, "y2": w.y2, "uuid": w.uuid }))
        .collect();

    let connected_labels: Vec<serde_json::Value> = labels
        .iter()
        .filter(|l| connected_nets.contains(&l.net))
        .map(|l| json!({ "net": l.net, "type": format!("{:?}", l.kind), "x": l.x, "y": l.y }))
        .collect();

    // Find other components on the same nets (excluding the queried one)
    let connected_components: Vec<serde_json::Value> = instances
        .iter()
        .filter(|i| i.reference != reference)
        .filter_map(|i| {
            let ls = find_lib_symbol(&lib_syms, i)?;
            let t = i.pin_transform();
            let matching_pins: Vec<_> = extract_lib_pins_for_unit(ls, i.unit)
                .iter()
                .filter_map(|p| {
                    let (px, py) = pin_endpoint(p, t);
                    if all_net_pts.contains(&pt_key(px, py)) {
                        Some(json!({ "pin": p.number, "name": p.name, "unit": i.unit }))
                    } else {
                        None
                    }
                })
                .collect();
            if matching_pins.is_empty() {
                None
            } else {
                Some(json!({
                    "reference": i.reference,
                    "value": i.value,
                    "unit": i.unit,
                    "connected_pins": matching_pins
                }))
            }
        })
        .collect();

    Ok(CallToolResult::json(&json!({
        "reference": reference,
        "nets": connected_nets.iter().collect::<Vec<_>>(),
        "connected_wires": connected_wires.len(),
        "wires": connected_wires,
        "labels": connected_labels,
        "connected_components": connected_components
    })))
}

async fn handle_check_overlaps(
    args: &serde_json::Value,
    _ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let tol = opt_f64(args, "tolerance").unwrap_or(0.5);
    let sch = cse::Schematic::load(&sch_path)?;

    // Component overlap detection using the new crate's spatial query
    let symbols: Vec<&cse::Symbol> = sch.symbols.iter().collect();
    let mut comp_overlaps: Vec<serde_json::Value> = Vec::new();
    for (i, a) in symbols.iter().enumerate() {
        let (ax, ay) = a.position();
        for b in &symbols[i + 1..] {
            let (bx, by) = b.position();
            if points_coincident(ax, ay, bx, by, tol) {
                comp_overlaps.push(json!({
                    "type": "component_overlap",
                    "a": a.reference().unwrap_or("?"),
                    "b": b.reference().unwrap_or("?"),
                    "x": ax, "y": ay
                }));
            }
        }
    }

    // Label overlap detection — collect all label types into a uniform list
    struct LabelInfo {
        net: String,
        x: f64,
        y: f64,
    }
    let mut all_labels: Vec<LabelInfo> = Vec::new();
    for l in sch.labels.iter() {
        all_labels.push(LabelInfo {
            net: l.text.clone(),
            x: l.at.x,
            y: l.at.y,
        });
    }
    for g in sch.global_labels.iter() {
        all_labels.push(LabelInfo {
            net: g.text.clone(),
            x: g.at.x,
            y: g.at.y,
        });
    }
    for h in sch.hierarchical_labels.iter() {
        all_labels.push(LabelInfo {
            net: h.text.clone(),
            x: h.at.x,
            y: h.at.y,
        });
    }
    let mut label_overlaps: Vec<serde_json::Value> = Vec::new();
    for (i, a) in all_labels.iter().enumerate() {
        for b in &all_labels[i + 1..] {
            if points_coincident(a.x, a.y, b.x, b.y, tol) && a.net != b.net {
                label_overlaps.push(json!({ "type": "label_overlap", "net_a": a.net, "net_b": b.net, "x": a.x, "y": a.y }));
            }
        }
    }

    let mut all = comp_overlaps;
    all.extend(label_overlaps);
    Ok(CallToolResult::json(
        &json!({ "overlap_count": all.len(), "overlaps": all }),
    ))
}

#[cfg(test)]
mod multi_unit_analysis_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::sync::Arc;

    /// Both units own a pin at the same local coordinate. They share a
    /// reference but are placed on different labelled points, which exposes
    /// any code that transforms unit 2 through unit 1's placement (#182).
    const SCHEMATIC: &str = r#"(kicad_sch
  (version 20260306)
  (uuid "root")
  (lib_symbols
    (symbol "Test:DUAL"
      (symbol "DUAL_1_1"
        (pin input line (at 0 0 0) (length 0) (name "A") (number "1"))
      )
      (symbol "DUAL_2_1"
        (pin output line (at 0 0 0) (length 0) (name "Y") (number "2"))
      )
    )
  )
  (label "UNIT1" (at 100 100 0))
  (label "UNIT2" (at 100 120 0))
  (symbol (lib_id "Test:DUAL") (at 100 100 0) (unit 1) (uuid "u1a")
    (property "Reference" "U1" (at 100 100 0))
    (property "Value" "DUAL" (at 100 100 0))
  )
  (symbol (lib_id "Test:DUAL") (at 100 120 0) (unit 2) (uuid "u1b")
    (property "Reference" "U1" (at 100 120 0))
    (property "Value" "DUAL" (at 100 120 0))
  )
  (symbol (lib_id "Test:DUAL") (at 200 100 0) (unit 1) (uuid "u2a")
    (property "Reference" "U2" (at 200 100 0))
    (property "Value" "DUAL" (at 200 100 0))
  )
  (symbol (lib_id "Test:DUAL") (at 100 120 0) (unit 2) (uuid "u2b")
    (property "Reference" "U2" (at 100 120 0))
    (property "Value" "DUAL" (at 100 120 0))
  )
)
"#;

    fn context() -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(crate::router::ToolRouter::new()),
        )
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("multi.kicad_sch");
        std::fs::write(&path, SCHEMATIC).unwrap();
        (directory, path)
    }

    fn body(result: CallToolResult) -> serde_json::Value {
        assert!(!result.is_error, "analysis unexpectedly failed");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn pin_query_finds_the_unit_that_owns_the_pin() {
        let (_directory, path) = fixture();
        let result = body(
            handle_get_pin_connections(
                &json!({
                    "schematic": path,
                    "reference": "U1",
                    "pin_number": "2"
                }),
                &context(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(result["pin_y"], 120.0);
        assert_eq!(result["net"], "UNIT2");
    }

    #[tokio::test]
    async fn component_query_reports_each_units_real_pin_and_net() {
        let (_directory, path) = fixture();
        let result = body(
            handle_get_component_nets(&json!({ "schematic": path, "reference": "U1" }), &context())
                .await
                .unwrap(),
        );
        let pins = result["pins"].as_array().unwrap();
        assert_eq!(pins.len(), 2, "one pin per placed unit: {result}");
        assert!(pins
            .iter()
            .any(|pin| pin["pin"] == "1" && pin["unit"] == 1 && pin["net"] == "UNIT1"));
        assert!(pins
            .iter()
            .any(|pin| pin["pin"] == "2" && pin["unit"] == 2 && pin["net"] == "UNIT2"));
    }

    #[tokio::test]
    async fn net_query_never_reports_another_units_phantom_pin() {
        let (_directory, path) = fixture();
        let result = body(
            handle_get_net_components(&json!({ "schematic": path, "net": "UNIT2" }), &context())
                .await
                .unwrap(),
        );
        let u1 = result["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["reference"] == "U1")
            .unwrap();
        assert_eq!(u1["unit"], 2);
        assert_eq!(u1["pins"].as_array().unwrap().len(), 1);
        assert_eq!(u1["pins"][0]["pin"], "2");
    }

    #[tokio::test]
    async fn connected_items_traces_every_unit_and_classifies_peers_by_unit() {
        let (_directory, path) = fixture();
        let result = body(
            handle_get_connected_items(
                &json!({ "schematic": path, "reference": "U1" }),
                &context(),
            )
            .await
            .unwrap(),
        );
        let nets = result["nets"].as_array().unwrap();
        assert!(nets.contains(&json!("UNIT1")), "{result}");
        assert!(nets.contains(&json!("UNIT2")), "{result}");

        let peer = result["connected_components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["reference"] == "U2")
            .unwrap();
        assert_eq!(peer["unit"], 2);
        assert_eq!(peer["connected_pins"].as_array().unwrap().len(), 1);
        assert_eq!(peer["connected_pins"][0]["pin"], "2");
    }
}

#[cfg(test)]
mod orphan_item_tests {
    use super::*;
    use crate::tools::ServerConfig;
    use std::io::Write;
    use std::sync::Arc;

    /// Run the registered tool against a temporary schematic, exactly as the
    /// MCP dispatch layer does after selecting its `ToolDef`.
    async fn call_result(schematic: &str, mut args: serde_json::Value) -> CallToolResult {
        let mut file = tempfile::NamedTempFile::with_suffix(".kicad_sch").unwrap();
        file.write_all(schematic.as_bytes()).unwrap();
        file.flush().unwrap();

        args["schematic"] = json!(file.path().to_str().unwrap());
        let definition = tools()
            .into_iter()
            .find(|tool| tool.name == "find_orphan_items")
            .unwrap();
        let context = ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: String::new(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(crate::router::ToolRouter::new()),
        );
        (definition.handler)(&args, Arc::new(context))
            .await
            .unwrap()
    }

    async fn call(schematic: &str, args: serde_json::Value) -> serde_json::Value {
        let result = call_result(schematic, args).await;
        assert!(!result.is_error, "find_orphan_items failed");
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(text).unwrap()
    }

    /// One wire from (90,100) to a zero-length pin of U1 at (100,100), a `SIG`
    /// label mid-segment, a stray `ORPHAN` label, and U2 with nothing on its pin.
    fn schematic(extra: &str) -> String {
        format!(
            r#"(kicad_sch
  (version 20260306)
  (generator "eeschema")
  (uuid "root")
  (lib_symbols
    (symbol "Test:P"
      (symbol "P_1_1"
        (pin passive line (at 0 0 0) (length 0) (name "A") (number "1"))
      )
    )
  )
  (wire (pts (xy 90 100) (xy 100 100)) (uuid "w1"))
  (label "SIG" (at 95 100 0))
  (label "ORPHAN" (at 200 200 0))
  (symbol (lib_id "Test:P") (at 100 100 0) (unit 1) (uuid "u1")
    (property "Reference" "U1" (at 100 100 0))
    (property "Value" "P" (at 100 100 0))
  )
  (symbol (lib_id "Test:P") (at 150 150 0) (unit 1) (uuid "u2")
    (property "Reference" "U2" (at 150 150 0))
    (property "Value" "P" (at 150 150 0))
  )
{extra}  (sheet_instances (path "/" (page "1")))
)
"#
        )
    }

    async fn orphans(extra: &str) -> Vec<serde_json::Value> {
        let body = call(&schematic(extra), json!({})).await;
        body["orphans"].as_array().unwrap().clone()
    }

    fn of_type<'a>(items: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
        items.iter().filter(|item| item["type"] == kind).collect()
    }

    #[tokio::test]
    async fn a_wire_ending_on_a_pin_is_not_dangling() {
        let items = orphans("").await;
        let dangling = of_type(&items, "dangling_wire_end");
        assert_eq!(dangling.len(), 1, "only the free end counts: {items:?}");
        assert_eq!(dangling[0]["x"], 90.0);
        assert_eq!(dangling[0]["wire_uuid"], "w1");
    }

    #[tokio::test]
    async fn a_label_mid_segment_is_not_floating() {
        let items = orphans("").await;
        let floating = of_type(&items, "floating_label");
        assert_eq!(floating.len(), 1, "only ORPHAN floats: {items:?}");
        assert_eq!(floating[0]["net"], "ORPHAN");
    }

    #[tokio::test]
    async fn a_pin_with_nothing_on_it_is_reported() {
        let items = orphans("").await;
        let pins = of_type(&items, "unconnected_pin");
        assert_eq!(pins.len(), 1, "U1's pin is wired: {items:?}");
        assert_eq!(pins[0]["reference"], "U2");
        assert_eq!(pins[0]["pin"], "1");
        assert_eq!(pins[0]["pin_name"], "A");
    }

    /// The reported #249 case: a label directly on a pin without a wire is a
    /// legal KiCAD connection and connects both items.
    #[tokio::test]
    async fn a_label_on_a_bare_pin_connects_both_items() {
        let items = orphans("  (label \"NC_SIG\" (at 150 150 0))\n").await;
        let floating = of_type(&items, "floating_label");
        assert_eq!(floating.len(), 1, "NC_SIG is on U2's pin: {items:?}");
        assert_eq!(floating[0]["net"], "ORPHAN");
        assert!(
            of_type(&items, "unconnected_pin").is_empty(),
            "the label connects U2: {items:?}"
        );
    }

    #[tokio::test]
    async fn a_no_connect_flag_exempts_its_pin() {
        let items = orphans("  (no_connect (at 150 150) (uuid \"nc1\"))\n").await;
        assert!(
            of_type(&items, "unconnected_pin").is_empty(),
            "no-connect covers U2: {items:?}"
        );
    }

    #[tokio::test]
    async fn an_intrinsically_no_connect_pin_is_exempt() {
        let no_connect_pin = schematic("").replace(
            "(pin passive line (at 0 0 0)",
            "(pin no_connect line (at 0 0 0)",
        );
        let body = call(&no_connect_pin, json!({})).await;
        let items = body["orphans"].as_array().unwrap();
        assert!(
            of_type(items, "unconnected_pin").is_empty(),
            "library no-connect pins are intentional: {items:?}"
        );
    }

    /// A multi-unit symbol must contribute only the pins from the placed unit.
    #[tokio::test]
    async fn another_unit_does_not_contribute_a_phantom_pin() {
        let schematic = r#"(kicad_sch
  (version 20260306)
  (generator "eeschema")
  (uuid "root")
  (lib_symbols
    (symbol "Test:D"
      (symbol "D_1_1"
        (pin passive line (at 0 0 0) (length 0) (name "A") (number "1"))
      )
      (symbol "D_2_1"
        (pin passive line (at 10 0 0) (length 0) (name "B") (number "2"))
      )
    )
  )
  (wire (pts (xy 90 100) (xy 100 100)) (uuid "w1"))
  (label "SIG" (at 90 100 0))
  (symbol (lib_id "Test:D") (at 100 100 0) (unit 1) (uuid "u1")
    (property "Reference" "U1" (at 100 100 0))
    (property "Value" "D" (at 100 100 0))
  )
  (sheet_instances (path "/" (page "1")))
)
"#;
        let body = call(schematic, json!({})).await;
        assert_eq!(body["orphan_count"], 0, "unit 2 is not placed here: {body}");
    }

    #[tokio::test]
    async fn a_wire_ending_on_a_sheet_pin_is_not_dangling() {
        let sheet = r#"  (sheet (at 80 95) (size 10 10) (uuid "s1")
    (property "Sheetname" "sub" (at 80 95 0))
    (property "Sheetfile" "sub.kicad_sch" (at 80 95 0))
    (pin "SIG" input (at 90 100 180) (uuid "sp1"))
  )
"#;
        let items = orphans(sheet).await;
        assert!(
            of_type(&items, "dangling_wire_end").is_empty(),
            "both ends terminate: {items:?}"
        );
    }

    #[tokio::test]
    async fn tolerance_must_be_positive() {
        for tolerance in [0.0, -0.05] {
            let result = call_result(&schematic(""), json!({ "tolerance": tolerance })).await;
            assert!(result.is_error, "accepted tolerance {tolerance}");
        }
    }
}
