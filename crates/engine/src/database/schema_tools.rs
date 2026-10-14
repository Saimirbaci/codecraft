//! DataGrip's schema tools (track DA): Create/Modify Table, Drop/Rename/Truncate with a preview,
//! the SQL Generator and Copy DDL, the ER diagram (model and automatic layout, pure and tested),
//! Compare Schemas with a migration script, and the rename-column follow-up that offers to update
//! the project's `.sql` files.
//!
//! Every database round trip runs on its own thread with its own connection (through the data
//! source's SSH tunnel when it has one) and reports through this module's channel, drained by
//! [`pump`]. DDL scripts run in one transaction where the dialect allows it
//! ([`codecraft_db::ddl::run_script`]); after a change the schema tree is reloaded.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::Serialize;

use codecraft_db::catalog;
use codecraft_db::ddl::{self, DdlScript, ObjectKind, ObjectRef, TableDef};
use codecraft_db::schema_diff::{self, SchemaDiff, SchemaModel};
use codecraft_db::{Connection, DataSourceConfig, DbKind, SecretStore};
use codecraft_lsp::lsp_types as lt;
use codecraft_workspace::NotificationLevel;

use super::{CONSOLES_DIR, ConnStatus, DbState, MetaReq, OutputLevel, node_key};
use crate::Session;

// ---------------------------------------------------------------- state

/// Create Table or Modify Table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EditorMode {
    Create,
    Modify,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "camelCase")]
pub enum ToolState {
    /// Reading from the database.
    Loading,
    Ready,
    /// A production data source: Execute asks first.
    Confirm,
    Running,
    /// What was done.
    Done(String),
    Failed(String),
}

impl ToolState {
    pub fn is_busy(&self) -> bool {
        matches!(self, ToolState::Loading | ToolState::Running)
    }
}

/// The Create/Modify Table dialog. The UI edits `table` in place; the SQL preview is
/// [`TableEditor::script`] of it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableEditor {
    pub mode: EditorMode,
    pub source: String,
    pub source_name: String,
    pub kind: DbKind,
    pub production: bool,
    pub schema: String,
    /// The table as the database has it (Modify).
    pub original: Option<TableDef>,
    pub table: TableDef,
    pub state: ToolState,
    /// Tables of the schema, for foreign key targets.
    pub tables: Vec<String>,
    #[serde(skip)]
    nonce: u64,
    /// Execute as soon as the table is read (headless Modify Table).
    #[serde(skip)]
    execute_when_loaded: Option<bool>,
}

impl TableEditor {
    /// The statements Execute runs (or why the definition is not valid yet).
    pub fn script(&self) -> Result<DdlScript, String> {
        let mut t = self.table.clone();
        t.schema = self.schema.clone();
        let r = match (self.mode, &self.original) {
            (EditorMode::Create, _) => ddl::create_table(self.kind, &t),
            (EditorMode::Modify, Some(old)) => ddl::alter_table(self.kind, old, &t),
            (EditorMode::Modify, None) => return Err("reading the table…".into()),
        };
        r.map_err(|e| e.to_string())
    }

    /// The SQL preview: the script, or the problem as a comment.
    pub fn preview(&self) -> String {
        match self.script() {
            Ok(s) if s.is_empty() => "-- No changes".into(),
            Ok(s) => s.to_sql(),
            Err(e) => format!("-- {e}"),
        }
    }

    pub fn title(&self) -> String {
        match self.mode {
            EditorMode::Create => {
                format!("Create Table in {}", if self.schema.is_empty() { &self.source_name } else { &self.schema })
            }
            EditorMode::Modify => {
                format!("Modify Table {}", self.original.as_ref().map(|t| t.name.as_str()).unwrap_or(&self.table.name))
            }
        }
    }
}

/// Drop, Rename or Truncate (edited in place by the prompt: the new name, cascade).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum SchemaOp {
    Drop {
        cascade: bool,
    },
    #[serde(rename_all = "camelCase")]
    Rename {
        new_name: String,
        update_sql_files: bool,
    },
    Truncate {
        cascade: bool,
    },
}

/// The confirmation of a Drop/Rename/Truncate, with its SQL.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaOpPrompt {
    pub op: SchemaOp,
    pub object: ObjectRef,
    pub source: String,
    pub source_name: String,
    pub kind: DbKind,
    pub production: bool,
    pub state: ToolState,
    #[serde(skip)]
    nonce: u64,
}

impl SchemaOpPrompt {
    pub fn title(&self) -> String {
        let what = self.object.kind.label();
        match &self.op {
            SchemaOp::Drop { .. } => format!("Drop {what} {}", self.object.display()),
            SchemaOp::Rename { .. } => format!("Rename {what} {}", self.object.display()),
            SchemaOp::Truncate { .. } => format!("Truncate {}", self.object.display()),
        }
    }

    pub fn script(&self) -> Result<DdlScript, String> {
        match &self.op {
            SchemaOp::Drop { cascade } => ddl::drop_object(self.kind, &self.object, *cascade),
            SchemaOp::Rename { new_name, .. } => ddl::rename_object(self.kind, &self.object, new_name.trim()),
            SchemaOp::Truncate { cascade } => {
                ddl::truncate_table(self.kind, &self.object.schema, &self.object.name, *cascade)
            }
        }
        .map_err(|e| e.to_string())
    }

    pub fn preview(&self) -> String {
        match self.script() {
            Ok(s) => s.to_sql(),
            Err(e) => format!("-- {e}"),
        }
    }
}

/// The SQL Generator window (and Copy DDL, which only copies).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedDdl {
    pub title: String,
    pub source: String,
    pub objects: Vec<ObjectRef>,
    pub state: ToolState,
    pub sql: String,
    /// Shown in the SQL Generator window (Copy DDL only copies).
    pub visible: bool,
    #[serde(skip)]
    copy: bool,
    #[serde(skip)]
    nonce: u64,
}

/// One side of a comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaSide {
    pub data_source: String,
    pub schema: String,
    /// `data source name / schema`.
    pub label: String,
}

/// Compare Schemas: the diff tree and the migration script that makes the target like the source.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareView {
    pub source: SchemaSide,
    pub target: SchemaSide,
    pub state: ToolState,
    pub diff: SchemaDiff,
    pub script: Option<DdlScript>,
    /// The script as text (or the reason there is none).
    pub sql: String,
    pub target_kind: DbKind,
    pub production: bool,
    #[serde(skip)]
    nonce: u64,
}

// ---------------------------------------------------------------- ER diagram

/// Layout metrics shared with the UI (diagram units: 1 = one pixel at zoom 1).
pub const DIAGRAM_HEADER: f32 = 26.0;
pub const DIAGRAM_ROW: f32 = 18.0;
pub const DIAGRAM_CHAR: f32 = 7.0;
/// Columns listed per table (the rest is summarised).
pub const DIAGRAM_MAX_COLUMNS: usize = 30;
const H_GAP: f32 = 90.0;
const V_GAP: f32 = 36.0;
const MAX_COLUMN_HEIGHT: f32 = 1400.0;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagramColumn {
    pub name: String,
    pub data_type: String,
    pub primary_key: bool,
    pub foreign_key: bool,
    pub nullable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagramNode {
    pub name: String,
    pub columns: Vec<DiagramColumn>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl DiagramNode {
    /// Rows drawn: the columns up to the limit, plus a "… more" row.
    pub fn rows(&self) -> usize {
        self.columns.len().min(DIAGRAM_MAX_COLUMNS) + usize::from(self.columns.len() > DIAGRAM_MAX_COLUMNS)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }

    /// The y of a column's row centre (the header for columns not shown).
    pub fn column_y(&self, column: &str) -> f32 {
        match self.columns.iter().take(DIAGRAM_MAX_COLUMNS).position(|c| c.name == column) {
            Some(i) => self.y + DIAGRAM_HEADER + DIAGRAM_ROW * (i as f32 + 0.5),
            None => self.y + DIAGRAM_HEADER / 2.0,
        }
    }
}

/// A foreign key: `from` (the referencing table) points at `to`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagramEdge {
    pub from: usize,
    pub to: usize,
    pub name: String,
    pub columns: Vec<String>,
    pub ref_columns: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagram {
    pub source: String,
    pub schema: String,
    pub title: String,
    pub state: ToolState,
    /// Only these tables (empty: the whole schema).
    pub only: Vec<String>,
    pub nodes: Vec<DiagramNode>,
    pub edges: Vec<DiagramEdge>,
    pub zoom: f32,
    /// Where the diagram's origin is drawn, in screen points from the view's top-left.
    pub pan: [f32; 2],
    pub selected: Option<String>,
    /// The view should fit the diagram once it is drawn (after loading or a new layout).
    pub fit_pending: bool,
    #[serde(skip)]
    nonce: u64,
}

impl Diagram {
    /// A ready diagram of the given tables (as laid out by the caller).
    pub fn from_parts(schema: &str, nodes: Vec<DiagramNode>, edges: Vec<DiagramEdge>) -> Self {
        Self {
            source: String::new(),
            schema: schema.to_string(),
            title: schema.to_string(),
            state: ToolState::Ready,
            only: Vec::new(),
            nodes,
            edges,
            zoom: 1.0,
            pan: [0.0, 0.0],
            selected: None,
            fit_pending: false,
            nonce: 0,
        }
    }

    pub fn node_index(&self, name: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.name == name)
    }

    /// The tables a table references and the ones that reference it.
    pub fn related(&self, name: &str) -> BTreeSet<String> {
        let Some(i) = self.node_index(name) else { return BTreeSet::new() };
        self.edges
            .iter()
            .filter_map(|e| {
                if e.from == i {
                    Some(e.to)
                } else if e.to == i {
                    Some(e.from)
                } else {
                    None
                }
            })
            .filter(|j| *j != i)
            .filter_map(|j| self.nodes.get(j).map(|n| n.name.clone()))
            .collect()
    }

    /// `[min x, min y, max x, max y]` of the tables (zero when empty).
    pub fn bounds(&self) -> [f32; 4] {
        if self.nodes.is_empty() {
            return [0.0; 4];
        }
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for n in &self.nodes {
            b[0] = b[0].min(n.x);
            b[1] = b[1].min(n.y);
            b[2] = b[2].max(n.x + n.width);
            b[3] = b[3].max(n.y + n.height);
        }
        b
    }

    /// Zoom and pan that show the whole diagram in a `width` × `height` view.
    pub fn fit(&mut self, width: f32, height: f32) {
        let [x0, y0, x1, y1] = self.bounds();
        let (w, h) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
        let margin = 24.0;
        let zoom = ((width - 2.0 * margin) / w).min((height - 2.0 * margin) / h).clamp(0.1, 1.5);
        self.zoom = if zoom.is_finite() { zoom } else { 1.0 };
        self.pan = [
            margin + ((width - 2.0 * margin) - w * self.zoom).max(0.0) / 2.0 - x0 * self.zoom,
            margin + ((height - 2.0 * margin) - h * self.zoom).max(0.0) / 2.0 - y0 * self.zoom,
        ];
        self.fit_pending = false;
    }

    fn from_model(model: &SchemaModel, only: &[String]) -> (Vec<DiagramNode>, Vec<DiagramEdge>) {
        let tables: Vec<&TableDef> =
            model.tables.iter().filter(|t| only.is_empty() || only.iter().any(|o| o == &t.name)).collect();
        let mut nodes: Vec<DiagramNode> = tables
            .iter()
            .map(|t| {
                let fk_cols: BTreeSet<&str> =
                    t.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
                let columns: Vec<DiagramColumn> = t
                    .columns
                    .iter()
                    .map(|c| DiagramColumn {
                        name: c.name.clone(),
                        data_type: c.data_type.clone(),
                        primary_key: t.primary_key.contains(&c.name),
                        foreign_key: fk_cols.contains(c.name.as_str()),
                        nullable: c.nullable,
                    })
                    .collect();
                let mut node = DiagramNode { name: t.name.clone(), columns, x: 0.0, y: 0.0, width: 0.0, height: 0.0 };
                size_node(&mut node);
                node
            })
            .collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        let index: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, n)| (n.name.as_str(), i)).collect();
        let mut edges = Vec::new();
        for t in &tables {
            let Some(&from) = index.get(t.name.as_str()) else { continue };
            for f in &t.foreign_keys {
                let same_schema = f.ref_schema.is_empty() || f.ref_schema == model.schema;
                let Some(&to) = index.get(f.ref_table.as_str()).filter(|_| same_schema) else { continue };
                edges.push(DiagramEdge {
                    from,
                    to,
                    name: f.name.clone(),
                    columns: f.columns.clone(),
                    ref_columns: f.ref_columns.clone(),
                });
            }
        }
        (nodes, edges)
    }

    /// The diagram as an SVG document (Export).
    pub fn to_svg(&self) -> String {
        let [x0, y0, x1, y1] = self.bounds();
        let pad = 20.0;
        let (w, h) = (x1 - x0 + 2.0 * pad, y1 - y0 + 2.0 * pad);
        let tx = |x: f32| x - x0 + pad;
        let ty = |y: f32| y - y0 + pad;
        let mut s = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.0}\" height=\"{h:.0}\" viewBox=\"0 0 {w:.0} {h:.0}\" \
             font-family=\"Menlo, monospace\" font-size=\"12\">\n<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>\n"
        );
        for e in &self.edges {
            let (Some(a), Some(b)) = (self.nodes.get(e.from), self.nodes.get(e.to)) else { continue };
            let points = edge_points(a, b, e);
            let path: Vec<String> = points.iter().map(|(x, y)| format!("{:.1},{:.1}", tx(*x), ty(*y))).collect();
            s.push_str(&format!(
                "<polyline points=\"{}\" fill=\"none\" stroke=\"#7a8699\" stroke-width=\"1.2\"><title>{}</title></polyline>\n",
                path.join(" "),
                xml(&e.name)
            ));
            if let (Some(&(px, py)), Some(&(ex, ey))) = (points.get(points.len().saturating_sub(2)), points.last()) {
                let dir = if ex >= px { -1.0 } else { 1.0 };
                let _ = py;
                s.push_str(&format!(
                    "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" fill=\"#7a8699\"/>\n",
                    tx(ex),
                    ty(ey),
                    tx(ex + dir * 8.0),
                    ty(ey - 4.0),
                    tx(ex + dir * 8.0),
                    ty(ey + 4.0)
                ));
            }
        }
        for n in &self.nodes {
            let (x, y) = (tx(n.x), ty(n.y));
            s.push_str(&format!(
                "<g><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"4\" fill=\"#f7f8fa\" stroke=\"#a0a8b8\"/>\n",
                n.width, n.height
            ));
            s.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{DIAGRAM_HEADER}\" rx=\"4\" fill=\"#dfe6f3\" stroke=\"#a0a8b8\"/>\n",
                n.width
            ));
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-weight=\"bold\" fill=\"#1d2433\">{}</text>\n",
                x + 8.0,
                y + 17.0,
                xml(&n.name)
            ));
            for (i, c) in n.columns.iter().take(DIAGRAM_MAX_COLUMNS).enumerate() {
                let ry = y + DIAGRAM_HEADER + DIAGRAM_ROW * (i as f32) + 13.0;
                let mark = match (c.primary_key, c.foreign_key) {
                    (true, _) => "PK ",
                    (false, true) => "FK ",
                    _ => "   ",
                };
                s.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{ry:.1}\" fill=\"#1d2433\" xml:space=\"preserve\">{}{}</text>\
                     <text x=\"{:.1}\" y=\"{ry:.1}\" fill=\"#6b7385\" text-anchor=\"end\">{}</text>\n",
                    x + 8.0,
                    mark,
                    xml(&c.name),
                    x + n.width - 8.0,
                    xml(&c.data_type)
                ));
            }
            if n.columns.len() > DIAGRAM_MAX_COLUMNS {
                let ry = y + DIAGRAM_HEADER + DIAGRAM_ROW * DIAGRAM_MAX_COLUMNS as f32 + 13.0;
                s.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{ry:.1}\" fill=\"#6b7385\">… {} more</text>\n",
                    x + 8.0,
                    n.columns.len() - DIAGRAM_MAX_COLUMNS
                ));
            }
            s.push_str("</g>\n");
        }
        s.push_str("</svg>\n");
        s
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn size_node(n: &mut DiagramNode) {
    let longest = n
        .columns
        .iter()
        .take(DIAGRAM_MAX_COLUMNS)
        .map(|c| c.name.chars().count() + c.data_type.chars().count().min(28) + 6)
        .max()
        .unwrap_or(0)
        .max(n.name.chars().count() + 4);
    n.width = (longest as f32 * DIAGRAM_CHAR + 16.0).clamp(150.0, 460.0);
    n.height = DIAGRAM_HEADER + DIAGRAM_ROW * n.rows() as f32 + 6.0;
}

/// The polyline of an edge: from the referencing column's row out of the side facing the
/// referenced table, to the referenced column's row.
pub fn edge_points(from: &DiagramNode, to: &DiagramNode, e: &DiagramEdge) -> Vec<(f32, f32)> {
    let y1 = e.columns.first().map(|c| from.column_y(c)).unwrap_or(from.y + DIAGRAM_HEADER / 2.0);
    let y2 = e.ref_columns.first().map(|c| to.column_y(c)).unwrap_or(to.y + DIAGRAM_HEADER / 2.0);
    if std::ptr::eq(from, to) {
        // A self reference loops out of the right side.
        let x = from.x + from.width;
        return vec![(x, y1), (x + 24.0, y1), (x + 24.0, y2), (x, y2)];
    }
    let from_cx = from.x + from.width / 2.0;
    let to_cx = to.x + to.width / 2.0;
    if (from_cx - to_cx).abs() < (from.width + to.width) / 2.0 {
        // Stacked in one column: leave and enter on the left.
        let x = from.x.min(to.x) - 24.0;
        return vec![(from.x, y1), (x, y1), (x, y2), (to.x, y2)];
    }
    let (x1, x2) = if from_cx > to_cx { (from.x, to.x + to.width) } else { (from.x + from.width, to.x) };
    let mid = (x1 + x2) / 2.0;
    vec![(x1, y1), (mid, y1), (mid, y2), (x2, y2)]
}

/// Automatic layered layout: referenced tables to the left of the tables that reference them
/// (longest path layering), tables ordered within a layer by the barycenter of their neighbours,
/// tall layers wrapped into several columns, unconnected tables in a grid below.
pub fn layout(nodes: &mut [DiagramNode], edges: &[DiagramEdge]) {
    let n = nodes.len();
    if n == 0 {
        return;
    }
    let links: Vec<(usize, usize)> =
        edges.iter().filter(|e| e.from != e.to && e.from < n && e.to < n).map(|e| (e.from, e.to)).collect();
    let connected: Vec<bool> = (0..n).map(|i| links.iter().any(|(a, b)| *a == i || *b == i)).collect();
    // Layers: a table sits one layer right of everything it references (cycles stop growing at n).
    let mut layer = vec![0usize; n];
    for _ in 0..n {
        let mut changed = false;
        for (a, b) in &links {
            let want = layer.get(*b).copied().unwrap_or(0) + 1;
            if let Some(la) = layer.get_mut(*a)
                && *la < want
                && want < n
            {
                *la = want;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let layer_count = (0..n)
        .filter(|i| connected.get(*i).copied().unwrap_or(false))
        .map(|i| layer.get(i).copied().unwrap_or(0))
        .max()
        .map_or(0, |m| m + 1);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layer_count];
    for i in 0..n {
        if connected.get(i).copied().unwrap_or(false)
            && let Some(l) = layers.get_mut(layer.get(i).copied().unwrap_or(0))
        {
            l.push(i);
        }
    }
    // Barycenter ordering, a few sweeps left to right and back.
    let neighbours = |i: usize| {
        links.iter().filter_map(move |(a, b)| {
            if *a == i {
                Some(*b)
            } else if *b == i {
                Some(*a)
            } else {
                None
            }
        })
    };
    let mut pos = vec![0f32; n];
    let set_pos = |layers: &Vec<Vec<usize>>, pos: &mut Vec<f32>| {
        for l in layers {
            for (k, i) in l.iter().enumerate() {
                if let Some(p) = pos.get_mut(*i) {
                    *p = k as f32 / (l.len().max(1) as f32);
                }
            }
        }
    };
    set_pos(&layers, &mut pos);
    for sweep in 0..6 {
        let order: Vec<usize> = if sweep % 2 == 0 {
            (1..layer_count).collect()
        } else {
            (0..layer_count.saturating_sub(1)).rev().collect()
        };
        for li in order {
            let Some(l) = layers.get(li) else { continue };
            let mut keyed: Vec<(f32, usize)> = l
                .iter()
                .map(|&i| {
                    let ns: Vec<f32> = neighbours(i).filter_map(|j| pos.get(j).copied()).collect();
                    let bc = if ns.is_empty() {
                        pos.get(i).copied().unwrap_or(0.0)
                    } else {
                        ns.iter().sum::<f32>() / ns.len() as f32
                    };
                    (bc, i)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            if let Some(l) = layers.get_mut(li) {
                *l = keyed.into_iter().map(|(_, i)| i).collect();
            }
            set_pos(&layers, &mut pos);
        }
    }
    // Columns: each layer, wrapped when taller than the limit.
    let heights: Vec<f32> = nodes.iter().map(|n| n.height).collect();
    let widths: Vec<f32> = nodes.iter().map(|n| n.width).collect();
    let height = |i: usize| heights.get(i).copied().unwrap_or(0.0);
    let width = |i: usize| widths.get(i).copied().unwrap_or(0.0);
    let mut columns: Vec<Vec<usize>> = Vec::new();
    for l in &layers {
        let mut col: Vec<usize> = Vec::new();
        let mut h = 0.0;
        for &i in l {
            if !col.is_empty() && h + height(i) > MAX_COLUMN_HEIGHT {
                columns.push(std::mem::take(&mut col));
                h = 0.0;
            }
            h += height(i) + V_GAP;
            col.push(i);
        }
        if !col.is_empty() {
            columns.push(col);
        }
    }
    let col_height = |c: &Vec<usize>| c.iter().map(|i| height(*i) + V_GAP).sum::<f32>() - V_GAP;
    let tallest = columns.iter().map(col_height).fold(0.0f32, f32::max);
    let mut x = 0.0;
    let mut bottom: f32 = 0.0;
    for c in &columns {
        let w = c.iter().map(|i| width(*i)).fold(0.0f32, f32::max);
        let mut y = (tallest - col_height(c)).max(0.0) / 2.0;
        for &i in c {
            if let Some(node) = nodes.get_mut(i) {
                node.x = x + (w - node.width) / 2.0;
                node.y = y;
                y += node.height + V_GAP;
                bottom = bottom.max(node.y + node.height);
            }
        }
        x += w + H_GAP;
    }
    // Unconnected tables: a grid under the rest (or alone).
    let lonely: Vec<usize> = (0..n).filter(|i| !connected.get(*i).copied().unwrap_or(false)).collect();
    if lonely.is_empty() {
        return;
    }
    let per_row = ((lonely.len() as f32).sqrt().ceil() as usize).max(1);
    let mut y = if columns.is_empty() { 0.0 } else { bottom + V_GAP * 2.0 };
    for row in lonely.chunks(per_row) {
        let mut x = 0.0;
        let mut row_h: f32 = 0.0;
        for &i in row {
            if let Some(node) = nodes.get_mut(i) {
                node.x = x;
                node.y = y;
                x += node.width + H_GAP / 2.0;
                row_h = row_h.max(node.height);
            }
        }
        y += row_h + V_GAP;
    }
}

// ---------------------------------------------------------------- the tools' state

enum Job {
    TableLoaded { nonce: u64, result: Result<(TableDef, Vec<String>), String> },
    Ran { nonce: u64, result: Result<usize, String> },
    Generated { nonce: u64, result: Result<String, String> },
    DiagramLoaded { nonce: u64, result: Result<SchemaModel, String> },
    Compared { nonce: u64, result: Result<(SchemaModel, SchemaModel), String> },
    SqlFiles { nonce: u64, result: Result<Vec<(PathBuf, String)>, String> },
}

/// A column rename whose `.sql` usages are being looked for.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlFileRename {
    pub source: String,
    pub table: String,
    pub old: String,
    pub new: String,
    pub state: ToolState,
    #[serde(skip)]
    nonce: u64,
}

/// What a finished script was for.
#[derive(Clone, Debug)]
enum RunFor {
    Editor,
    Prompt,
    Compare,
}

/// Everything the schema tools keep (in [`DbState::schema`]).
pub struct SchemaTools {
    pub table_editor: Option<TableEditor>,
    pub prompt: Option<SchemaOpPrompt>,
    pub generated: Option<GeneratedDdl>,
    pub diagram: Option<Diagram>,
    pub compare: Option<CompareView>,
    pub sql_rename: Option<SqlFileRename>,
    /// A PNG export the UI renders (`db.diagram.export` with `format: png`).
    pub png_request: Option<PathBuf>,
    runs: HashMap<u64, (RunFor, String)>,
    tx: Sender<Job>,
    rx: Receiver<Job>,
}

impl Default for SchemaTools {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            table_editor: None,
            prompt: None,
            generated: None,
            diagram: None,
            compare: None,
            sql_rename: None,
            png_request: None,
            runs: HashMap::new(),
            tx,
            rx,
        }
    }
}

impl SchemaTools {
    pub fn busy(&self) -> bool {
        !self.runs.is_empty()
            || self.table_editor.as_ref().is_some_and(|e| e.state.is_busy())
            || self.generated.as_ref().is_some_and(|g| g.state.is_busy())
            || self.diagram.as_ref().is_some_and(|d| d.state.is_busy())
            || self.compare.as_ref().is_some_and(|c| c.state.is_busy())
            || self.sql_rename.as_ref().is_some_and(|r| r.state.is_busy())
    }
}

/// A connection to open on a worker thread.
struct ConnPlan {
    cfg: DataSourceConfig,
    tunnel: Option<crate::ssh_tunnel::TunnelPlan>,
    password: Option<String>,
    key: String,
}

fn open(plan: ConnPlan, secrets: &Arc<dyn SecretStore>) -> Result<Box<dyn Connection>, String> {
    let cfg = crate::ssh_tunnel::route(plan.tunnel, plan.cfg)?;
    let pw = match plan.password {
        Some(p) => Some(p),
        None if cfg.password_saved => secrets.get(&cfg.id).map_err(|e| e.to_string())?,
        None => None,
    };
    codecraft_db::connect(&cfg, pw.as_deref()).map_err(|e| e.to_string())
}

impl DbState {
    fn plan(&mut self, source: &str, purpose: &str) -> Result<ConnPlan, String> {
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        let cfg = self.resolved(&ds.config);
        let key = format!("{purpose}:{}", self.next());
        let tunnel = self.plan_tunnel(&cfg)?.map(|t| t.keyed(key.clone()));
        Ok(ConnPlan { cfg, tunnel, password: self.worker_password(source), key })
    }

    /// Open connections for `sources` on a new thread and hand them to `job`.
    fn spawn_job(
        &mut self,
        sources: &[&str],
        purpose: &str,
        job: impl FnOnce(Result<Vec<Box<dyn Connection>>, String>) -> Job + Send + 'static,
    ) -> Result<(), String> {
        let mut plans = Vec::new();
        for s in sources {
            plans.push(self.plan(s, purpose)?);
        }
        let keys: Vec<String> = plans.iter().map(|p| p.key.clone()).collect();
        let tunnels = self.tunnels.clone();
        let secrets = self.secrets.clone();
        let events = self.schema.tx.clone();
        std::thread::Builder::new()
            .name(format!("db-{purpose}"))
            .spawn(move || {
                let conns: Result<Vec<_>, String> = plans.into_iter().map(|p| open(p, &secrets)).collect();
                let ev = job(conns);
                for k in &keys {
                    tunnels.close(Some(k));
                }
                let _ = events.send(ev);
            })
            .map_err(|e| format!("cannot start a database thread: {e}"))?;
        Ok(())
    }

    fn writable(&self, source: &str) -> Result<(String, DbKind, bool), String> {
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        if ds.config.read_only {
            return Err(format!("{} is read-only", ds.config.name));
        }
        Ok((ds.config.name.clone(), ds.config.kind, ds.config.confirm_writes))
    }

    /// Tables of a loaded schema node (for pickers).
    pub fn schema_tables(&self, source: &str, schema: &str) -> Vec<String> {
        self.source(source)
            .and_then(|d| d.tree.schemas.get(schema))
            .and_then(|n| n.tables.as_ref())
            .map(|t| {
                t.iter()
                    .filter(|t| t.info.kind == codecraft_db::TableKind::Table)
                    .map(|t| t.info.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The schema a data source uses when none is named (its default, or the only one).
    pub fn default_schema(&self, source: &str) -> String {
        let Some(ds) = self.source(source) else { return String::new() };
        if let Some(info) = &ds.tree.info {
            if let Some(d) = &info.default_schema {
                return d.clone();
            }
            if let [only] = info.schemas.as_slice() {
                return only.clone();
            }
        }
        match ds.config.kind {
            DbKind::Sqlite => "main".into(),
            DbKind::Postgres => "public".into(),
            DbKind::Mysql | DbKind::Mariadb => ds.config.database.clone(),
        }
    }

    // ------------------------------------------------------------ Create / Modify Table

    /// Open Create Table for `schema` (optionally starting from `table`).
    pub(crate) fn open_create_table(
        &mut self,
        source: &str,
        schema: &str,
        table: Option<TableDef>,
    ) -> Result<(), String> {
        let (source_name, kind, production) = self.writable(source)?;
        let mut t = table.unwrap_or_else(|| TableDef {
            name: "new_table".into(),
            columns: vec![ddl::ColumnDef {
                name: "id".into(),
                data_type: match kind {
                    DbKind::Sqlite => "INTEGER".into(),
                    DbKind::Postgres => "integer".into(),
                    DbKind::Mysql | DbKind::Mariadb => "int".into(),
                },
                nullable: false,
                ..Default::default()
            }],
            primary_key: vec!["id".into()],
            ..Default::default()
        });
        t.schema = schema.to_string();
        let nonce = self.next();
        self.schema.table_editor = Some(TableEditor {
            mode: EditorMode::Create,
            source: source.to_string(),
            source_name,
            kind,
            production,
            schema: schema.to_string(),
            original: None,
            table: t,
            state: ToolState::Ready,
            tables: self.schema_tables(source, schema),
            nonce,
            execute_when_loaded: None,
        });
        Ok(())
    }

    /// Open Modify Table: the table's definition is read in the background. With `then`, the
    /// definition replaces the loaded one (keeping column identities by `original`), and
    /// `Some(confirm)` executes right away.
    pub(crate) fn open_modify_table(
        &mut self,
        source: &str,
        schema: &str,
        table: &str,
        then: Option<(TableDef, Option<bool>)>,
    ) -> Result<(), String> {
        let (source_name, kind, production) = self.writable(source)?;
        let nonce = self.next();
        let (s2, t2) = (schema.to_string(), table.to_string());
        self.spawn_job(&[source], "schema-read", move |conns| {
            let result = conns.and_then(|mut c| {
                let conn = c.first_mut().ok_or("no connection")?;
                let def = catalog::table_def(conn.as_mut(), &s2, &t2).map_err(|e| e.to_string())?;
                let tables = conn
                    .tables(&s2)
                    .map(|t| {
                        t.into_iter().filter(|t| t.kind == codecraft_db::TableKind::Table).map(|t| t.name).collect()
                    })
                    .unwrap_or_default();
                Ok((def, tables))
            });
            Job::TableLoaded { nonce, result }
        })?;
        let (start, execute) = match then {
            Some((t, e)) => (t, e),
            None => (TableDef { schema: schema.into(), name: table.into(), ..Default::default() }, None),
        };
        self.schema.table_editor = Some(TableEditor {
            mode: EditorMode::Modify,
            source: source.to_string(),
            source_name,
            kind,
            production,
            schema: schema.to_string(),
            original: None,
            table: start,
            state: ToolState::Loading,
            tables: Vec::new(),
            nonce,
            execute_when_loaded: execute,
        });
        Ok(())
    }

    /// Run the editor's script (asks first on a production data source unless `confirmed`).
    /// Returns the statements started, or `None` when it waits for confirmation.
    pub(crate) fn execute_table_editor(&mut self, confirmed: bool) -> Result<Option<usize>, String> {
        let e = self.schema.table_editor.as_ref().ok_or("no table dialog is open")?;
        if e.state.is_busy() {
            return Err("the table is still being read or changed".into());
        }
        let script = e.script()?;
        if script.is_empty() {
            return Err("nothing to change".into());
        }
        let source = e.source.clone();
        self.writable(&source)?;
        if e.production && !confirmed {
            if let Some(e) = self.schema.table_editor.as_mut() {
                e.state = ToolState::Confirm;
            }
            return Ok(None);
        }
        let n = script.statements.len();
        let title = e.title();
        self.run_script_job(&source, script, RunFor::Editor, title)?;
        if let Some(e) = self.schema.table_editor.as_mut() {
            e.state = ToolState::Running;
        }
        Ok(Some(n))
    }

    fn run_script_job(&mut self, source: &str, script: DdlScript, what: RunFor, title: String) -> Result<u64, String> {
        let nonce = self.next();
        // SQLite: open read cursors of other connections would block the change.
        self.close_cursors("", source);
        self.log(OutputLevel::Sql, source, script.to_sql());
        self.spawn_job(&[source], "schema-ddl", move |conns| {
            let result = conns.and_then(|mut c| {
                let conn = c.first_mut().ok_or("no connection")?;
                ddl::run_script(conn.as_mut(), &script).map_err(|f| f.message())
            });
            Job::Ran { nonce, result }
        })?;
        self.schema.runs.insert(nonce, (what, title));
        Ok(nonce)
    }

    // ------------------------------------------------------------ Drop / Rename / Truncate

    pub(crate) fn open_schema_op(&mut self, source: &str, object: ObjectRef, op: SchemaOp) -> Result<(), String> {
        let (source_name, kind, production) = self.writable(source)?;
        // Refuse what the dialect cannot do before asking (a rename is checked with a stand-in name).
        let probe_op = match &op {
            SchemaOp::Rename { new_name, update_sql_files } if new_name.trim().is_empty() => {
                SchemaOp::Rename { new_name: "new_name".into(), update_sql_files: *update_sql_files }
            }
            other => other.clone(),
        };
        let probe = SchemaOpPrompt {
            op: probe_op,
            object: object.clone(),
            source: source.to_string(),
            source_name: source_name.clone(),
            kind,
            production,
            state: ToolState::Ready,
            nonce: 0,
        };
        probe.script()?;
        let nonce = self.next();
        self.schema.prompt = Some(SchemaOpPrompt {
            op,
            object,
            source: source.to_string(),
            source_name,
            kind,
            production,
            state: ToolState::Ready,
            nonce,
        });
        Ok(())
    }

    /// Run the prompt's statements.
    pub(crate) fn confirm_schema_op(&mut self) -> Result<usize, String> {
        let p = self.schema.prompt.as_ref().ok_or("nothing to confirm")?;
        if p.state.is_busy() {
            return Err("already running".into());
        }
        let script = p.script()?;
        let (source, title) = (p.source.clone(), p.title());
        self.writable(&source)?;
        let n = script.statements.len();
        let nonce = self.run_script_job(&source, script, RunFor::Prompt, title)?;
        if let Some(p) = self.schema.prompt.as_mut() {
            p.state = ToolState::Running;
            p.nonce = nonce;
        }
        Ok(n)
    }

    // ------------------------------------------------------------ SQL Generator / Copy DDL

    pub(crate) fn generate_ddl(&mut self, source: &str, objects: Vec<ObjectRef>, copy: bool) -> Result<(), String> {
        if objects.is_empty() {
            return Err("no objects selected".into());
        }
        let nonce = self.next();
        let list = objects.clone();
        self.spawn_job(&[source], "schema-ddl-gen", move |conns| {
            let result = conns.and_then(|mut c| {
                let conn = c.first_mut().ok_or("no connection")?;
                let mut out = String::new();
                for o in &list {
                    let ddl = catalog::object_ddl(conn.as_mut(), o).map_err(|e| format!("{}: {e}", o.display()))?;
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&ddl);
                }
                Ok(out)
            });
            Job::Generated { nonce, result }
        })?;
        let title = match objects.as_slice() {
            [one] => format!("DDL of {} {}", one.kind.label(), one.display()),
            many => format!("DDL of {} objects", many.len()),
        };
        self.schema.generated = Some(GeneratedDdl {
            title,
            source: source.to_string(),
            objects,
            state: ToolState::Loading,
            sql: String::new(),
            visible: !copy,
            copy,
            nonce,
        });
        Ok(())
    }

    // ------------------------------------------------------------ ER diagram

    pub(crate) fn show_diagram(&mut self, source: &str, schema: &str, only: Vec<String>) -> Result<(), String> {
        let name =
            self.source(source).map(|d| d.config.name.clone()).ok_or_else(|| format!("no data source `{source}`"))?;
        let nonce = self.next();
        let (s2, filter) = (schema.to_string(), only.clone());
        self.spawn_job(&[source], "schema-diagram", move |conns| {
            let result = conns.and_then(|mut c| {
                let conn = c.first_mut().ok_or("no connection")?;
                let only = (!filter.is_empty()).then_some(filter.as_slice());
                catalog::schema_model(conn.as_mut(), &s2, only).map_err(|e| e.to_string())
            });
            Job::DiagramLoaded { nonce, result }
        })?;
        self.schema.diagram = Some(Diagram {
            source: source.to_string(),
            schema: schema.to_string(),
            title: if only.is_empty() {
                format!("{name}: {schema}")
            } else {
                format!("{name}: {schema} ({} tables)", only.len())
            },
            state: ToolState::Loading,
            only,
            nodes: Vec::new(),
            edges: Vec::new(),
            zoom: 1.0,
            pan: [20.0, 20.0],
            selected: None,
            fit_pending: true,
            nonce,
        });
        Ok(())
    }

    // ------------------------------------------------------------ Compare

    pub(crate) fn compare_schemas(&mut self, source: SchemaSide, target: SchemaSide) -> Result<(), String> {
        let tds = self.source(&target.data_source).ok_or_else(|| format!("no data source `{}`", target.data_source))?;
        let (target_kind, production) = (tds.config.kind, tds.config.confirm_writes);
        self.source(&source.data_source).ok_or_else(|| format!("no data source `{}`", source.data_source))?;
        let nonce = self.next();
        let (ss, ts) = (source.schema.clone(), target.schema.clone());
        let sources = [source.data_source.as_str(), target.data_source.as_str()];
        self.spawn_job(&sources, "schema-compare", move |conns| {
            let result = conns.and_then(|mut c| {
                let mut it = c.iter_mut();
                let a = it.next().ok_or("no connection")?;
                let sm = catalog::schema_model(a.as_mut(), &ss, None).map_err(|e| format!("{ss}: {e}"))?;
                let b = it.next().ok_or("no connection")?;
                let tm = catalog::schema_model(b.as_mut(), &ts, None).map_err(|e| format!("{ts}: {e}"))?;
                Ok((sm, tm))
            });
            Job::Compared { nonce, result }
        })?;
        self.schema.compare = Some(CompareView {
            source,
            target,
            state: ToolState::Loading,
            diff: SchemaDiff::default(),
            script: None,
            sql: String::new(),
            target_kind,
            production,
            nonce,
        });
        Ok(())
    }

    /// Run the comparison's migration on the target.
    pub(crate) fn apply_compare(&mut self, confirmed: bool) -> Result<Option<usize>, String> {
        let c = self.schema.compare.as_ref().ok_or("no comparison is open")?;
        if c.state.is_busy() {
            return Err("the comparison is still running".into());
        }
        let script = c.script.clone().ok_or_else(|| c.sql.clone())?;
        if script.is_empty() {
            return Err("the schemas are already the same".into());
        }
        let target = c.target.data_source.clone();
        self.writable(&target)?;
        if c.production && !confirmed {
            if let Some(c) = self.schema.compare.as_mut() {
                c.state = ToolState::Confirm;
            }
            return Ok(None);
        }
        let n = script.statements.len();
        let title = format!("Migrate {}", c.target.label);
        self.run_script_job(&target, script, RunFor::Compare, title)?;
        if let Some(c) = self.schema.compare.as_mut() {
            c.state = ToolState::Running;
        }
        Ok(Some(n))
    }

    // ------------------------------------------------------------ refresh

    /// Reload what the tree shows of a data source after a schema change.
    fn refresh_tree(&mut self, source: &str) {
        let Some(ds) = self.source(source) else { return };
        if !matches!(ds.status, ConnStatus::Connected(_)) {
            return;
        }
        let mut reqs = vec![MetaReq::Info];
        for (schema, node) in &ds.tree.schemas {
            if let Some(tables) = &node.tables {
                reqs.push(MetaReq::Tables(schema.clone()));
                for t in tables.iter().filter(|t| t.details.is_some()) {
                    reqs.push(MetaReq::Details(schema.clone(), t.info.name.clone()));
                }
            }
        }
        let prefix = node_key(source, Some(""), None);
        self.table_keys.retain(|k, _| !k.starts_with(&prefix));
        for r in reqs {
            let _ = self.meta(source, r);
        }
    }
}

/// Write `sql` into a new console of `source` and open it.
pub(crate) fn open_in_console(s: &mut Session, source: &str, sql: &str) -> Result<PathBuf, String> {
    let root = s.db.project_root().map(Path::to_path_buf).ok_or("open a project to create SQL consoles")?;
    let dir = root.join(CONSOLES_DIR).join(source);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut path = dir.join("console.sql");
    let mut n = 1;
    while path.exists() && n < 10_000 {
        path = dir.join(format!("console_{n}.sql"));
        n += 1;
    }
    codecraft_fs::write_text_file_atomic(&path, sql).map_err(|e| e.to_string())?;
    s.open_file(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

// ---------------------------------------------------------------- rename in SQL files

/// Char ranges of `name` used as an identifier in `text` (bare, case-insensitive, or quoted
/// exactly), outside strings and comments. The flag says whether the match is inside quotes.
pub fn identifier_spans(text: &str, name: &str) -> Vec<(usize, usize, Option<char>)> {
    let chars: Vec<char> = text.chars().collect();
    let target: Vec<char> = name.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        if c == '\'' {
            i += 1;
            while let Some(&d) = chars.get(i) {
                i += 1;
                if d == '\'' {
                    if chars.get(i) == Some(&'\'') {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
        } else if c == '-' && next == Some('-') {
            while chars.get(i).is_some_and(|d| *d != '\n') {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len() && !(chars.get(i) == Some(&'*') && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '"' || c == '`' {
            let start = i + 1;
            let mut name_chars = Vec::new();
            i += 1;
            let mut simple = true;
            while let Some(&d) = chars.get(i) {
                i += 1;
                if d == c {
                    if chars.get(i) == Some(&c) {
                        simple = false;
                        name_chars.push(c);
                        i += 1;
                        continue;
                    }
                    break;
                }
                name_chars.push(d);
            }
            if simple && name_chars == target {
                out.push((start, start + target.len(), Some(c)));
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while chars.get(i).is_some_and(|d| d.is_alphanumeric() || *d == '_' || *d == '$') {
                i += 1;
            }
            let word = chars.get(start..i).unwrap_or_default();
            if word.len() == target.len()
                && word.iter().zip(&target).all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            {
                out.push((start, i, None));
            }
        } else {
            i += 1;
        }
    }
    out
}

/// The text edits that rename column `old` to `new` in the statements of `text` that mention
/// `table`. Positions are char offsets with the replacement text.
pub fn column_rename_edits(kind: DbKind, text: &str, table: &str, old: &str, new: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    for st in codecraft_db::split_statements(text, kind.dialect()) {
        if identifier_spans(&st.text, table).is_empty() {
            continue;
        }
        let base = st.start;
        for (a, b, quoted) in identifier_spans(&st.text, old) {
            let replacement = match quoted {
                Some(q) => {
                    let doubled: String = new.chars().flat_map(|c| if c == q { vec![c, c] } else { vec![c] }).collect();
                    doubled
                }
                None => {
                    let plain = new.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                        && new.chars().all(|c| c.is_alphanumeric() || c == '_');
                    if plain { new.to_string() } else { codecraft_db::sql::quote_ident(kind, new) }
                }
            };
            out.push((base + a, base + b, replacement));
        }
    }
    out
}

impl DbState {
    /// Look for `.sql` files of the project that use `table`.`old` (in the background).
    fn find_sql_usages(&mut self, source: &str, table: &str, old: &str, new: &str) -> Result<(), String> {
        let root = self.project_root().map(Path::to_path_buf).ok_or("no project")?;
        let nonce = self.next();
        let events = self.schema.tx.clone();
        let needle = table.to_string();
        std::thread::Builder::new()
            .name("db-sql-usages".into())
            .spawn(move || {
                let result = (|| {
                    let re = regex::RegexBuilder::new(&regex::escape(&needle))
                        .case_insensitive(true)
                        .build()
                        .map_err(|e| e.to_string())?;
                    let opts = codecraft_fs::FindOptions {
                        file_mask: Some("*.sql".into()),
                        max_results: 50_000,
                        ..Default::default()
                    };
                    let cancel = std::sync::atomic::AtomicBool::new(false);
                    let hits = codecraft_fs::find_in_files(&root, &re, &opts, &cancel).map_err(|e| e.to_string())?;
                    let files: BTreeSet<PathBuf> = hits.into_iter().map(|m| m.path).collect();
                    Ok(files
                        .into_iter()
                        .take(crate::refactor::preview::MAX_ENTRIES)
                        .filter_map(|p| codecraft_fs::read_text_file(&p).ok().map(|t| (p, t)))
                        .collect())
                })();
                let _ = events.send(Job::SqlFiles { nonce, result });
            })
            .map_err(|e| format!("cannot search the SQL files: {e}"))?;
        self.schema.sql_rename = Some(SqlFileRename {
            source: source.to_string(),
            table: table.to_string(),
            old: old.to_string(),
            new: new.to_string(),
            state: ToolState::Loading,
            nonce,
        });
        Ok(())
    }
}

// `lt::WorkspaceEdit::changes` is keyed by `Uri` (its interior cache never changes the key).
#[allow(clippy::mutable_key_type)]
fn sql_files_found(s: &mut Session, nonce: u64, result: Result<Vec<(PathBuf, String)>, String>) {
    let Some(r) = s.db.schema.sql_rename.clone().filter(|r| r.nonce == nonce) else { return };
    let kind = s.db.source(&r.source).map(|d| d.config.kind).unwrap_or_default();
    let files = match result {
        Ok(f) => f,
        Err(e) => {
            if let Some(x) = s.db.schema.sql_rename.as_mut() {
                x.state = ToolState::Failed(e.clone());
            }
            s.status = format!("Cannot search the SQL files: {e}");
            return;
        }
    };
    let mut changes: HashMap<lt::Uri, Vec<lt::TextEdit>> = HashMap::new();
    for (path, disk) in files {
        // An open document may have unsaved edits: use its text.
        let text = s.ws.doc_by_path(&path).and_then(|d| s.ws.doc(d)).map(|d| d.doc.text()).unwrap_or(disk);
        let edits = column_rename_edits(kind, &text, &r.table, &r.old, &r.new);
        if edits.is_empty() {
            continue;
        }
        let doc = codecraft_text::Document::from_text(&text);
        let lsp: Vec<lt::TextEdit> = edits
            .into_iter()
            .map(|(a, b, new_text)| {
                let (l1, c1) = doc.offset_to_utf16(a);
                let (l2, c2) = doc.offset_to_utf16(b);
                lt::TextEdit { range: lt::Range::new(lt::Position::new(l1, c1), lt::Position::new(l2, c2)), new_text }
            })
            .collect();
        if let Ok(uri) = codecraft_lsp::path_to_uri(&path) {
            changes.insert(uri, lsp);
        }
    }
    let n = changes.len();
    if let Some(x) = s.db.schema.sql_rename.as_mut() {
        x.state = ToolState::Done(format!("{n} file(s)"));
    }
    if n == 0 {
        s.status = format!("No SQL files use {}.{}", r.table, r.old);
        return;
    }
    let edit = lt::WorkspaceEdit { changes: Some(changes), ..Default::default() };
    let title = format!("Rename column {}.{} to {} in SQL files", r.table, r.old, r.new);
    if let Err(e) = crate::refactor::apply_or_preview(s, &title, &edit, true) {
        s.status = format!("{title}: {e}");
    }
}

// ---------------------------------------------------------------- pump

/// Drain the schema tools' events; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    for _ in 0..64 {
        let Ok(job) = s.db.schema.rx.try_recv() else { break };
        changed = true;
        handle(s, job);
    }
    changed
}

fn handle(s: &mut Session, job: Job) {
    match job {
        Job::TableLoaded { nonce, result } => {
            let Some(e) = s.db.schema.table_editor.as_mut().filter(|e| e.nonce == nonce) else { return };
            match result {
                Ok((def, tables)) => {
                    let loaded = def.for_modify();
                    let edited = std::mem::take(&mut e.table);
                    e.table = if edited.columns.is_empty() {
                        loaded.clone()
                    } else {
                        claim_columns(TableDef { schema: e.schema.clone(), ..edited }, &loaded)
                    };
                    e.original = Some(loaded);
                    e.tables = tables;
                    e.state = ToolState::Ready;
                    let execute = e.execute_when_loaded.take();
                    if let Some(confirm) = execute
                        && let Err(err) = s.db.execute_table_editor(confirm)
                        && let Some(e) = s.db.schema.table_editor.as_mut()
                    {
                        e.state = ToolState::Failed(err);
                    }
                }
                Err(err) => {
                    e.state = ToolState::Failed(err.clone());
                    s.status = format!("Cannot read the table: {err}");
                }
            }
        }
        Job::Ran { nonce, result } => ran(s, nonce, result),
        Job::Generated { nonce, result } => {
            let Some(g) = s.db.schema.generated.as_mut().filter(|g| g.nonce == nonce) else { return };
            match result {
                Ok(sql) => {
                    g.sql = sql.clone();
                    g.state = ToolState::Ready;
                    if g.copy {
                        let n = g.objects.len();
                        s.effects.push(crate::Effect::CopyToClipboard(sql));
                        s.status = format!("DDL of {n} object{} copied", if n == 1 { "" } else { "s" });
                    } else {
                        s.status = g.title.clone();
                    }
                }
                Err(e) => {
                    g.state = ToolState::Failed(e.clone());
                    g.visible = true;
                    s.status = format!("Cannot generate the DDL: {e}");
                }
            }
        }
        Job::DiagramLoaded { nonce, result } => {
            let Some(d) = s.db.schema.diagram.as_mut().filter(|d| d.nonce == nonce) else { return };
            match result {
                Ok(model) => {
                    let (mut nodes, edges) = Diagram::from_model(&model, &d.only);
                    layout(&mut nodes, &edges);
                    s.status = format!("{} tables, {} references", nodes.len(), edges.len());
                    d.nodes = nodes;
                    d.edges = edges;
                    d.state = ToolState::Ready;
                    d.fit_pending = true;
                }
                Err(e) => {
                    s.status = format!("Cannot read the schema: {e}");
                    d.state = ToolState::Failed(e);
                }
            }
        }
        Job::Compared { nonce, result } => {
            let Some(c) = s.db.schema.compare.as_mut().filter(|c| c.nonce == nonce) else { return };
            match result {
                Ok((src, dst)) => {
                    c.diff = schema_diff::diff(&src, &dst);
                    match schema_diff::migration(&src, &dst) {
                        Ok(script) => {
                            c.sql =
                                if script.is_empty() { "-- The schemas are the same".into() } else { script.to_sql() };
                            c.script = Some(script);
                        }
                        Err(e) => {
                            c.sql = format!("-- Cannot write the migration: {e}");
                            c.script = None;
                        }
                    }
                    let (a, r, ch) = c.diff.counts();
                    s.status = format!("Compared: {a} only in the source, {r} only in the target, {ch} changed");
                    c.state = ToolState::Ready;
                }
                Err(e) => {
                    s.status = format!("Cannot compare: {e}");
                    c.state = ToolState::Failed(e);
                }
            }
        }
        Job::SqlFiles { nonce, result } => sql_files_found(s, nonce, result),
    }
}

/// A definition given by an agent for Modify Table: columns named like an existing column that
/// no other column claims keep that column (and its data) instead of replacing it.
fn claim_columns(mut t: TableDef, loaded: &TableDef) -> TableDef {
    let claimed: BTreeSet<String> =
        t.columns.iter().filter(|c| !c.original.is_empty()).map(|c| c.original.clone()).collect();
    for c in t.columns.iter_mut().filter(|c| c.original.is_empty()) {
        if loaded.column(&c.name).is_some() && !claimed.contains(&c.name) {
            c.original = c.name.clone();
        }
    }
    t
}

/// A script finished: report exactly, refresh the tree, update the dialog that started it.
fn ran(s: &mut Session, nonce: u64, result: Result<usize, String>) {
    let Some((what, title)) = s.db.schema.runs.remove(&nonce) else { return };
    let source = match &what {
        RunFor::Editor => s.db.schema.table_editor.as_ref().map(|e| e.source.clone()),
        RunFor::Prompt => s.db.schema.prompt.as_ref().map(|p| p.source.clone()),
        RunFor::Compare => s.db.schema.compare.as_ref().map(|c| c.target.data_source.clone()),
    }
    .unwrap_or_default();
    match &result {
        Ok(n) => {
            let msg = format!("{title}: {n} statement{} executed", if *n == 1 { "" } else { "s" });
            s.db.log(OutputLevel::Info, &source, msg.clone());
            s.status = msg;
            s.db.refresh_tree(&source);
        }
        Err(e) => {
            s.db.log(OutputLevel::Error, &source, e.clone());
            s.status = format!("{title} failed");
            s.notify(NotificationLevel::Error, &format!("{title} failed"), e.clone());
            // A non-transactional script may have changed some things.
            s.db.refresh_tree(&source);
        }
    }
    match what {
        RunFor::Editor => {
            let Some(e) = s.db.schema.table_editor.as_mut() else { return };
            match result {
                Ok(_) => {
                    let done = format!("{} done", e.title());
                    // The dialog closes; agents see the outcome in the status and the log.
                    s.db.schema.table_editor = None;
                    s.status = done;
                }
                Err(err) => e.state = ToolState::Failed(err),
            }
        }
        RunFor::Prompt => {
            let Some(p) = s.db.schema.prompt.clone() else { return };
            match result {
                Ok(_) => {
                    s.db.schema.prompt = None;
                    if let SchemaOp::Rename { new_name, update_sql_files: true } = &p.op
                        && p.object.kind == ObjectKind::Column
                        && let Err(e) =
                            s.db.find_sql_usages(&p.source, &p.object.table, &p.object.name, new_name.trim())
                    {
                        s.status = format!("Cannot search the SQL files: {e}");
                    }
                }
                Err(err) => {
                    if let Some(p) = s.db.schema.prompt.as_mut() {
                        p.state = ToolState::Failed(err);
                    }
                }
            }
        }
        RunFor::Compare => {
            let Some(c) = s.db.schema.compare.as_mut() else { return };
            match result {
                Ok(n) => {
                    c.state = ToolState::Done(format!("{n} statement(s) executed on {}", c.target.label));
                }
                Err(err) => c.state = ToolState::Failed(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_db::ddl::{ColumnDef, ForeignKeyDef};

    fn table(name: &str, refs: &[&str]) -> TableDef {
        TableDef {
            name: name.into(),
            columns: vec![ColumnDef { name: "id".into(), data_type: "integer".into(), ..Default::default() }],
            primary_key: vec!["id".into()],
            foreign_keys: refs
                .iter()
                .map(|r| ForeignKeyDef {
                    name: format!("{name}_{r}"),
                    columns: vec!["id".into()],
                    ref_table: (*r).into(),
                    ref_columns: vec!["id".into()],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn diagram(tables: Vec<TableDef>) -> Diagram {
        let model = SchemaModel { kind: DbKind::Sqlite, schema: "main".into(), tables, views: Vec::new() };
        let (mut nodes, edges) = Diagram::from_model(&model, &[]);
        layout(&mut nodes, &edges);
        Diagram {
            source: "s".into(),
            schema: "main".into(),
            title: String::new(),
            state: ToolState::Ready,
            only: Vec::new(),
            nodes,
            edges,
            zoom: 1.0,
            pan: [0.0, 0.0],
            selected: None,
            fit_pending: false,
            nonce: 0,
        }
    }

    fn overlaps(d: &Diagram) -> bool {
        d.nodes.iter().enumerate().any(|(i, a)| {
            d.nodes
                .iter()
                .skip(i + 1)
                .any(|b| a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height)
        })
    }

    #[test]
    fn layered_layout_puts_referenced_tables_left_without_overlaps() {
        let d = diagram(vec![
            table("orders", &["customers"]),
            table("customers", &[]),
            table("order_items", &["orders", "products"]),
            table("products", &["categories"]),
            table("categories", &["categories"]),
            table("audit", &[]),
        ]);
        let x = |n: &str| d.nodes[d.node_index(n).unwrap()].x;
        assert!(x("customers") < x("orders") && x("orders") < x("order_items"));
        assert!(x("categories") < x("products") && x("products") < x("order_items"));
        assert!(!overlaps(&d));
        // The unconnected table sits below the layered part.
        let audit = &d.nodes[d.node_index("audit").unwrap()];
        assert!(d.nodes.iter().filter(|n| n.name != "audit").all(|n| audit.y > n.y + n.height - 1.0));
        assert_eq!(d.related("orders"), BTreeSet::from(["customers".to_string(), "order_items".to_string()]));
        assert_eq!(d.related("categories"), BTreeSet::from(["products".to_string()]));
        assert!(d.related("nope").is_empty());
        assert_eq!(d.edges.len(), 5);
    }

    #[test]
    fn layout_handles_cycles_big_schemas_and_empty_ones() {
        let d = diagram(vec![table("a", &["b"]), table("b", &["a"])]);
        assert!(!overlaps(&d));
        let many: Vec<TableDef> =
            (0..120).map(|i| table(&format!("t{i:03}"), if i > 0 { &["t000"] } else { &[] })).collect();
        let d = diagram(many);
        assert!(!overlaps(&d));
        let [_, _, _, max_y] = d.bounds();
        assert!(max_y < 3000.0, "tall layers wrap into columns: {max_y}");
        let mut empty = diagram(Vec::new());
        assert_eq!(empty.bounds(), [0.0; 4]);
        empty.fit(800.0, 600.0);
        assert!(empty.zoom.is_finite());
    }

    #[test]
    fn fit_and_svg() {
        let mut d = diagram(vec![table("we<ird>&\"", &[]), table("b", &["we<ird>&\""])]);
        d.fit(400.0, 300.0);
        assert!(d.zoom > 0.0 && d.zoom <= 1.5);
        let [x0, y0, x1, y1] = d.bounds();
        let (sx0, sy0) = (x0 * d.zoom + d.pan[0], y0 * d.zoom + d.pan[1]);
        let (sx1, sy1) = (x1 * d.zoom + d.pan[0], y1 * d.zoom + d.pan[1]);
        assert!(sx0 >= 0.0 && sy0 >= 0.0 && sx1 <= 400.5 && sy1 <= 300.5, "{sx0} {sy0} {sx1} {sy1}");
        let svg = d.to_svg();
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("we&lt;ird&gt;&amp;&quot;"));
        assert!(!svg.contains("we<ird>"));
        assert!(svg.contains("<polyline"));
    }

    #[test]
    fn rename_edits_only_touch_statements_of_the_table() {
        let text = "select name, 'name' from users; -- name\nselect name from other;\nupdate \"users\" set \"name\" = 1 where Name is null;";
        let edits = column_rename_edits(DbKind::Postgres, text, "users", "name", "full name");
        let mut out: Vec<char> = text.chars().collect();
        for (a, b, r) in edits.iter().rev() {
            out.splice(*a..*b, r.chars());
        }
        let out: String = out.into_iter().collect();
        assert_eq!(
            out,
            "select \"full name\", 'name' from users; -- name\nselect name from other;\nupdate \"users\" set \"full name\" = 1 where \"full name\" is null;"
        );
        assert!(column_rename_edits(DbKind::Mysql, "select ünï from t", "t", "ünï", "x").len() == 1);
        assert!(identifier_spans("select names from t", "name").is_empty());
    }
}
