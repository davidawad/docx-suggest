//! Render a `DiffDoc` as tracked changes (`w:ins` / `w:del`) inside B's own
//! `word/document.xml`. Only paragraphs that carry a change or a comment
//! anchor are rewritten; every other byte of the part, and every other part
//! of the package, is copied through untouched.

use crate::comments::{Resolved, Target};
use docdiff::diff::{self, DiffDoc, OpKind, ParaKind};
use docdiff::model::{
    Atom, Item, ParaModel, Parsed, atom_count, is_transparent, parse_paragraph, run_items,
};
use docdiff::ooxml::{self, Elem, Node, esc_attr, esc_text, parse_fragment};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct Opts {
    /// Revision and comment author (`w:author`).
    pub author: String,
    /// Comment initials; derived from `author` when `None`.
    pub initials: Option<String>,
    /// Author email: recorded in `word/people.xml` so Word / ONLYOFFICE
    /// tie the author name to an identity. `None` = no people part.
    pub email: Option<String>,
    pub date: String,
}

#[derive(Debug, Clone)]
pub enum Ev {
    Del { a_para: usize, a_start: usize, len: usize },
    CStart(usize),
    CEnd(usize),
}

#[derive(Debug, Default, Clone)]
pub struct Plan {
    /// Per B atom: true = inserted.
    pub kinds: Vec<bool>,
    /// (position, rank, event); emitted before the atom at `position`.
    pub events: Vec<(usize, i32, Ev)>,
    pub para_insert: bool,
}

impl Plan {
    fn touched(&self) -> bool {
        self.para_insert || !self.events.is_empty() || self.kinds.iter().any(|k| *k)
    }
}

const RANK_END: i32 = -5;
const RANK_START: i32 = 0;
const RANK_DEL: i32 = 10;
const RANK_START_LATE: i32 = 15;

/// Everything the document rewrite needs, derived from diff + notes.
pub struct Planned {
    pub plans: Vec<Plan>,
    /// Comments anchored to wholly deleted paragraphs: ParaDiff index -> ids.
    pub deleted_para_comments: Vec<(usize, Vec<usize>)>,
}

pub fn plan(diff: &DiffDoc, b: &[ParaModel], notes: &[Resolved]) -> Result<Planned> {
    let mut plans: Vec<Plan> = b
        .iter()
        .map(|p| Plan {
            kinds: vec![false; p.atoms.len()],
            ..Plan::default()
        })
        .collect();

    for pd in &diff.paragraphs {
        let Some(j) = pd.b_para else { continue };
        let plan = &mut plans[j];
        match pd.kind {
            ParaKind::Insert => {
                plan.para_insert = true;
                plan.kinds.iter_mut().for_each(|k| *k = true);
            }
            ParaKind::Replace => {
                for op in &pd.ops {
                    let len = op.text.chars().count();
                    match op.kind {
                        OpKind::Insert => {
                            let end = op.b_start + len;
                            if end > plan.kinds.len() {
                                bail!("insert op past end of B paragraph {j}");
                            }
                            plan.kinds[op.b_start..end].iter_mut().for_each(|k| *k = true);
                        }
                        OpKind::Delete => plan.events.push((
                            op.b_start,
                            RANK_DEL,
                            Ev::Del {
                                a_para: pd.a_para.context("delete op without A paragraph")?,
                                a_start: op.a_start,
                                len,
                            },
                        )),
                        OpKind::Equal => {}
                    }
                }
            }
            ParaKind::Equal | ParaKind::Delete => {}
        }
    }

    let mut deleted_para_comments: Vec<(usize, Vec<usize>)> = Vec::new();
    for n in notes {
        match &n.target {
            Target::Text { b_para, start, end } => {
                let plan = &mut plans[*b_para];
                let late = !plan.kinds[*start];
                plan.events.push((
                    *start,
                    if late { RANK_START_LATE } else { RANK_START },
                    Ev::CStart(n.id),
                ));
                plan.events.push((*end, RANK_END, Ev::CEnd(n.id)));
            }
            Target::Change(_) | Target::Changes(..) => {
                let (lo, hi) = match n.target {
                    Target::Changes(lo, hi) => (lo, hi),
                    Target::Change(c) => (c, c),
                    Target::Text { .. } => unreachable!(),
                };
                let c = &lo;
                let hit = |o: &docdiff::diff::Op| o.change.is_some_and(|k| k >= lo && k <= hi);
                let (idx, pd) = diff
                    .paragraphs
                    .iter()
                    .enumerate()
                    .find(|(_, pd)| pd.ops.iter().any(hit))
                    .with_context(|| format!("no change #{c} in this diff ({} changes)", diff.changes))?;
                if pd.kind != ParaKind::Delete && (lo..=hi).any(|k| !pd.ops.iter().any(|o| o.change == Some(k))) {
                    bail!("changes #{lo}..#{hi} are not all in one paragraph");
                }
                match (pd.kind, pd.b_para) {
                    (ParaKind::Delete, _) => {
                        match deleted_para_comments.iter_mut().find(|(i, _)| *i == idx) {
                            Some((_, ids)) => ids.push(n.id),
                            None => deleted_para_comments.push((idx, vec![n.id])),
                        }
                    }
                    (_, Some(j)) => {
                        let ops: Vec<_> = pd.ops.iter().filter(|o| hit(o)).collect();
                        let (first, last) = (ops[0], ops[ops.len() - 1]);
                        let plan = &mut plans[j];
                        plan.events.push((first.b_start, RANK_START, Ev::CStart(n.id)));
                        let len = last.text.chars().count();
                        match last.kind {
                            OpKind::Insert => {
                                plan.events.push((last.b_start + len, RANK_END, Ev::CEnd(n.id)));
                            }
                            _ => plan.events.push((last.b_start, 20, Ev::CEnd(n.id))),
                        }
                    }
                    _ => bail!("change #{c} has no renderable paragraph"),
                }
            }
        }
    }
    plans.iter_mut().for_each(|p| p.events.sort_by_key(|e| (e.0, e.1)));
    Ok(Planned {
        plans,
        deleted_para_comments,
    })
}

// ------------------------------------------------------------ painter

pub struct Painter<'a> {
    pub opts: &'a Opts,
    pub a: &'a [ParaModel],
    pub style_ids: &'a HashSet<String>,
    pub next_id: &'a mut u64,
}

fn nodes(xml: &str) -> Result<Vec<Node>> {
    parse_fragment(xml.as_bytes())
}

impl Painter<'_> {
    fn rev_attrs(&mut self) -> String {
        let id = *self.next_id;
        *self.next_id += 1;
        format!(
            "w:id=\"{id}\" w:author=\"{}\" w:date=\"{}\"",
            esc_attr(&self.opts.author),
            esc_attr(&self.opts.date)
        )
    }

    fn wrap(&mut self, tag: &str, inner: &str) -> Result<Vec<Node>> {
        let attrs = self.rev_attrs();
        nodes(&format!("<w:{tag} {attrs}>{inner}</w:{tag}>"))
    }

    fn strip_missing_rstyle(&self, rpr: &[u8]) -> Vec<u8> {
        if rpr.is_empty() {
            return Vec::new();
        }
        let Ok(mut ns) = parse_fragment(rpr) else {
            return Vec::new();
        };
        for n in &mut ns {
            if let Node::Elem(e) = n {
                e.children.retain(|c| match c {
                    Node::Elem(s) if s.local() == "rStyle" => {
                        s.attr("val").is_some_and(|v| self.style_ids.contains(&v))
                    }
                    _ => true,
                });
            }
        }
        ooxml::to_bytes(&ns)
    }

    /// `<w:r>` runs (delText) for A atoms, grouped by run properties.
    fn deleted_runs(&self, a_para: usize, from: usize, len: usize) -> String {
        let m = &self.a[a_para];
        let slice = &m.atoms[from..from + len];
        let mut out = String::new();
        let mut i = 0;
        while i < slice.len() {
            let rpr_idx = slice[i].rpr;
            let j = slice[i..]
                .iter()
                .position(|a| a.rpr != rpr_idx)
                .map_or(slice.len(), |p| i + p);
            let rpr = self.strip_missing_rstyle(&m.rprs[rpr_idx]);
            out.push_str("<w:r>");
            out.push_str(&String::from_utf8_lossy(&rpr));
            out.push_str(&del_content(&slice[i..j]));
            out.push_str("</w:r>");
            i = j;
        }
        out
    }

    fn deletion(&mut self, a_para: usize, from: usize, len: usize) -> Result<Vec<Node>> {
        let runs = self.deleted_runs(a_para, from, len);
        self.wrap("del", &runs)
    }

    fn emit_events(&mut self, events: &mut Vec<(usize, i32, Ev)>, upto: usize, out: &mut Vec<Node>) -> Result<()> {
        while events.first().is_some_and(|e| e.0 <= upto) {
            let (_, _, ev) = events.remove(0);
            match ev {
                Ev::Del { a_para, a_start, len } => out.extend(self.deletion(a_para, a_start, len)?),
                Ev::CStart(id) => out.extend(nodes(&format!("<w:commentRangeStart w:id=\"{id}\"/>"))?),
                Ev::CEnd(id) => out.extend(nodes(&format!(
                    "<w:commentRangeEnd w:id=\"{id}\"/><w:r><w:commentReference w:id=\"{id}\"/></w:r>"
                ))?),
            }
        }
        Ok(())
    }

    fn piece_xml(run: &Elem, items: &[&Item]) -> String {
        let rpr = run
            .find_child("rPr")
            .map(|e| String::from_utf8_lossy(&ooxml::to_bytes(&[Node::Elem(e.clone())])).into_owned())
            .unwrap_or_default();
        let mut body = String::new();
        let mut text = String::new();
        let flush = |text: &mut String, body: &mut String| {
            if !text.is_empty() {
                body.push_str(&format!("<w:t xml:space=\"preserve\">{}</w:t>", esc_text(text)));
                text.clear();
            }
        };
        for it in items {
            match it {
                Item::Atom(c, None) => text.push(*c),
                Item::Atom(_, Some(raw)) => {
                    flush(&mut text, &mut body);
                    body.push_str(&String::from_utf8_lossy(raw));
                }
                Item::Pass(n) => {
                    flush(&mut text, &mut body);
                    body.push_str(&String::from_utf8_lossy(&ooxml::to_bytes(std::slice::from_ref(n))));
                }
            }
        }
        flush(&mut text, &mut body);
        format!("<{}>{rpr}{body}</w:r>", String::from_utf8_lossy(&run.start))
    }

    fn render_run(
        &mut self,
        plan: &Plan,
        events: &mut Vec<(usize, i32, Ev)>,
        run: &Elem,
        base: usize,
        out: &mut Vec<Node>,
    ) -> Result<()> {
        let items = run_items(run);
        let n_atoms = items.iter().filter(|i| matches!(i, Item::Atom(..))).count();
        let is_cut = |abs: usize, events: &[(usize, i32, Ev)]| {
            abs > base
                && (plan.kinds[abs] != plan.kinds[abs - 1] || events.iter().any(|e| e.0 == abs))
        };
        // Fast path: nothing inside this run changes.
        let uniform = (1..n_atoms).all(|k| !is_cut(base + k, events));
        if uniform && !plan.kinds[base] {
            out.push(Node::Elem(run.clone()));
            return Ok(());
        }
        let mut piece: Vec<&Item> = Vec::new();
        let mut piece_kind = plan.kinds[base];
        let mut k = 0usize;
        for it in &items {
            if matches!(it, Item::Atom(..)) {
                let abs = base + k;
                if k > 0 && is_cut(abs, events) {
                    self.flush_piece(run, &piece, piece_kind, out)?;
                    piece.clear();
                    self.emit_events(events, abs, out)?;
                    piece_kind = plan.kinds[abs];
                }
                k += 1;
            }
            piece.push(it);
        }
        self.flush_piece(run, &piece, piece_kind, out)
    }

    fn flush_piece(&mut self, run: &Elem, piece: &[&Item], inserted: bool, out: &mut Vec<Node>) -> Result<()> {
        if piece.is_empty() {
            return Ok(());
        }
        let xml = Self::piece_xml(run, piece);
        if inserted {
            out.extend(self.wrap("ins", &xml)?);
        } else {
            out.extend(nodes(&xml)?);
        }
        Ok(())
    }

    fn render_children(
        &mut self,
        plan: &Plan,
        events: &mut Vec<(usize, i32, Ev)>,
        children: &[Node],
        cursor: &mut usize,
    ) -> Result<Vec<Node>> {
        let mut out: Vec<Node> = Vec::new();
        for n in children {
            let Node::Elem(e) = n else {
                out.push(n.clone());
                continue;
            };
            match e.local() {
                "r" => {
                    let na = docdiff::model::atom_count_run(e);
                    if na == 0 {
                        out.push(n.clone());
                    } else {
                        self.emit_events(events, *cursor, &mut out)?;
                        self.render_run(plan, events, e, *cursor, &mut out)?;
                        *cursor += na;
                    }
                }
                "ins" | "moveTo" => {
                    *cursor += atom_count(n);
                    out.push(n.clone());
                }
                l if is_transparent(l) && atom_count(n) > 0 => {
                    // A replacement that starts at the wrapper's first atom
                    // (deleted text + inserted text) stays inside the wrapper,
                    // so a link's own text is diffed in place.
                    if !plan.kinds.get(*cursor).copied().unwrap_or(false) {
                        self.emit_events(events, *cursor, &mut out)?;
                    }
                    let inner = self.render_children(plan, events, &e.children, cursor)?;
                    out.push(Node::Elem(Elem {
                        name: e.name.clone(),
                        start: e.start.clone(),
                        empty: false,
                        children: inner,
                    }));
                }
                _ => out.push(n.clone()),
            }
        }
        Ok(self.merge_tracked(out))
    }

    /// Merge adjacent `w:ins` / `w:del` wrappers we created into one revision.
    fn merge_tracked(&self, nodes: Vec<Node>) -> Vec<Node> {
        let ours = |e: &Elem| {
            matches!(e.local(), "ins" | "del")
                && e.attr("author").as_deref() == Some(self.opts.author.as_str())
                && e.attr("date").as_deref() == Some(self.opts.date.as_str())
        };
        nodes.into_iter().fold(Vec::new(), |mut acc: Vec<Node>, n| {
            if let Node::Elem(cur) = &n
                && ours(cur)
                && let Some(Node::Elem(prev)) = acc.last_mut()
                && prev.name == cur.name
                && ours(prev)
            {
                prev.children.extend(cur.children.iter().cloned());
                return acc;
            }
            acc.push(n);
            acc
        })
    }

    pub fn render_paragraph(&mut self, p: &Elem, plan: &Plan) -> Result<Elem> {
        let mut events = plan.events.clone();
        let mut cursor = 0usize;
        let mut children = self.render_children(plan, &mut events, &p.children, &mut cursor)?;
        let mut tail = Vec::new();
        self.emit_events(&mut events, usize::MAX, &mut tail)?;
        children.extend(tail);
        let children = self.merge_tracked(children);
        let children = if plan.para_insert {
            let mark = format!("<w:ins {}/>", self.rev_attrs());
            with_para_mark(children, &mark)?
        } else {
            children
        };
        Ok(Elem {
            name: p.name.clone(),
            start: p.start.clone(),
            empty: false,
            children,
        })
    }

    /// A paragraph that exists only in A: all of its text as a deletion, with
    /// the paragraph mark deleted too. `comment_ids` anchor around the text.
    pub fn render_deleted_paragraph(&mut self, a_xml: &[u8], a_para: usize, comment_ids: &[usize]) -> Result<Vec<u8>> {
        let a_models = self.a;
        let m = &a_models[a_para];
        let p = parse_paragraph(a_xml, m.span)?;
        let mark = format!("<w:del {}/>", self.rev_attrs());
        let kept: Vec<Node> = p
            .find_child("pPr")
            .map(|ppr| {
                ppr.children
                    .iter()
                    .filter(|c| match c {
                        Node::Elem(e) => match e.local() {
                            "numPr" | "sectPr" | "rPr" | "pPrChange" => false,
                            "pStyle" => e.attr("val").is_some_and(|v| self.style_ids.contains(&v)),
                            _ => true,
                        },
                        _ => false,
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let ppr_inner = ooxml::to_bytes(&kept);
        let runs = self.deleted_runs(a_para, 0, m.atoms.len());
        let del = if runs.is_empty() {
            String::new()
        } else {
            let attrs = self.rev_attrs();
            format!("<w:del {attrs}>{runs}</w:del>")
        };
        let starts: String = comment_ids
            .iter()
            .map(|id| format!("<w:commentRangeStart w:id=\"{id}\"/>"))
            .collect();
        let ends: String = comment_ids
            .iter()
            .map(|id| {
                format!("<w:commentRangeEnd w:id=\"{id}\"/><w:r><w:commentReference w:id=\"{id}\"/></w:r>")
            })
            .collect();
        let xml = format!(
            "<w:p><w:pPr>{}<w:rPr>{mark}</w:rPr></w:pPr>{starts}{del}{ends}</w:p>",
            String::from_utf8_lossy(&ppr_inner)
        );
        Ok(xml.into_bytes())
    }
}

fn del_content(atoms: &[Atom]) -> String {
    let mut out = String::new();
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut String| {
        if !text.is_empty() {
            out.push_str(&format!("<w:delText xml:space=\"preserve\">{}</w:delText>", esc_text(text)));
            text.clear();
        }
    };
    for a in atoms {
        match (&a.raw, a.ch) {
            (Some(raw), _) => {
                flush(&mut text, &mut out);
                out.push_str(&String::from_utf8_lossy(raw));
            }
            (None, c) => text.push(c),
        }
    }
    flush(&mut text, &mut out);
    out
}

/// Put `mark` (`<w:ins .../>` or `<w:del .../>`) into the paragraph-mark
/// run properties of the paragraph, creating `pPr` / `rPr` as needed.
pub fn with_para_mark(mut children: Vec<Node>, mark: &str) -> Result<Vec<Node>> {
    let mark_nodes = nodes(mark)?;
    let ppr_idx = children
        .iter()
        .position(|n| matches!(n, Node::Elem(e) if e.local() == "pPr"));
    match ppr_idx {
        None => {
            let ppr = nodes(&format!("<w:pPr><w:rPr>{mark}</w:rPr></w:pPr>"))?;
            children.splice(0..0, ppr);
        }
        Some(i) => {
            let Node::Elem(ppr) = &mut children[i] else {
                bail!("pPr index mismatch");
            };
            ppr.empty = false;
            let rpr_pos = ppr
                .children
                .iter()
                .position(|c| matches!(c, Node::Elem(e) if e.local() == "rPr"));
            match rpr_pos {
                Some(k) => {
                    if let Node::Elem(rpr) = &mut ppr.children[k] {
                        rpr.empty = false;
                        rpr.children.splice(0..0, mark_nodes);
                    }
                }
                None => {
                    let at = ppr
                        .children
                        .iter()
                        .position(|c| matches!(c, Node::Elem(e) if matches!(e.local(), "sectPr" | "pPrChange")))
                        .unwrap_or(ppr.children.len());
                    let rpr = nodes(&format!("<w:rPr>{mark}</w:rPr>"))?;
                    ppr.children.splice(at..at, rpr);
                }
            }
        }
    }
    Ok(children)
}

/// Where wholly deleted A paragraphs go: `(ParaDiff index, A index)` pairs
/// placed before B's first paragraph (`leading`) or right after the B
/// paragraph that precedes them in the diff (`after[j]`).
pub type Slot = (usize, usize);

pub fn delete_slots(diff: &DiffDoc, n_b: usize) -> (Vec<Slot>, Vec<Vec<Slot>>) {
    let mut leading: Vec<Slot> = Vec::new();
    let mut after: Vec<Vec<Slot>> = vec![Vec::new(); n_b];
    let mut prev: Option<usize> = None;
    for (idx, pd) in diff.paragraphs.iter().enumerate() {
        match (pd.kind, pd.a_para, pd.b_para) {
            (ParaKind::Delete, Some(ai), _) => match prev {
                Some(j) => after[j].push((idx, ai)),
                None => leading.push((idx, ai)),
            },
            (_, _, Some(j)) => prev = Some(j),
            _ => {}
        }
    }
    (leading, after)
}

/// Paragraph texts that reject-all must reproduce: A's text in B's layout,
/// with excluded B paragraphs standing for themselves.
pub fn expected_reject(diff: &DiffDoc, a: &[ParaModel], b: &[ParaModel]) -> Vec<String> {
    let (leading, after) = delete_slots(diff, b.len());
    let a_of_b: std::collections::HashMap<usize, usize> = diff
        .paragraphs
        .iter()
        .filter_map(|pd| Some((pd.b_para?, pd.a_para?)))
        .collect();
    let mut out: Vec<String> = leading.iter().map(|(_, ai)| a[*ai].text.clone()).collect();
    for (j, bp) in b.iter().enumerate() {
        let is_diffed = diff.paragraphs.iter().any(|pd| pd.b_para == Some(j));
        match (is_diffed, a_of_b.get(&j)) {
            (true, Some(ai)) => out.push(a[*ai].text.clone()),
            (true, None) => {}
            (false, _) => out.push(bp.text.clone()),
        }
        out.extend(after[j].iter().map(|(_, ai)| a[*ai].text.clone()));
    }
    out
}

/// Splice rewritten paragraphs (and wholly deleted A paragraphs) into B's
/// `document.xml`; returns the new bytes.
pub fn render_document(
    b: &Parsed,
    a: &Parsed,
    diff: &DiffDoc,
    planned: &Planned,
    opts: &Opts,
    style_ids: &HashSet<String>,
    first_rev_id: u64,
) -> Result<Vec<u8>> {
    let mut next_id = first_rev_id;
    let mut painter = Painter {
        opts,
        a: &a.paras,
        style_ids,
        next_id: &mut next_id,
    };

    let (leading, after) = delete_slots(diff, b.paras.len());

    let mut out: Vec<u8> = Vec::with_capacity(b.xml.len() + b.xml.len() / 8);
    let mut cursor = 0usize;
    let comment_ids_for = |idx: usize| -> Vec<usize> {
        planned
            .deleted_para_comments
            .iter()
            .find(|(i, _)| *i == idx)
            .map(|(_, ids)| ids.clone())
            .unwrap_or_default()
    };

    for &(idx, ai) in &leading {
        let first = b.paras.first().context("B has no paragraphs")?.span.0;
        out.extend_from_slice(&b.xml[cursor..first]);
        cursor = first;
        out.extend(painter.render_deleted_paragraph(&a.xml, ai, &comment_ids_for(idx))?);
    }
    for (j, pm) in b.paras.iter().enumerate() {
        out.extend_from_slice(&b.xml[cursor..pm.span.0]);
        let plan = &planned.plans[j];
        if plan.touched() {
            let p = parse_paragraph(&b.xml, pm.span)?;
            let rendered = painter.render_paragraph(&p, plan)?;
            out.extend(ooxml::to_bytes(&[Node::Elem(rendered)]));
        } else {
            out.extend_from_slice(&b.xml[pm.span.0..pm.span.1]);
        }
        cursor = pm.span.1;
        for &(idx, ai) in &after[j] {
            out.extend(painter.render_deleted_paragraph(&a.xml, ai, &comment_ids_for(idx))?);
        }
    }
    out.extend_from_slice(&b.xml[cursor..]);
    Ok(out)
}

// -------------------------------------------------------- verification

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Accept,
    Reject,
}

fn walk_text(nodes: &[Node], mode: Mode, in_ins: bool, in_del: bool, out: &mut String) {
    for n in nodes {
        let Node::Elem(e) = n else { continue };
        match e.local() {
            "ins" | "moveTo" => walk_text(&e.children, mode, true, in_del, out),
            "del" | "moveFrom" => walk_text(&e.children, mode, in_ins, true, out),
            "r" => {
                let visible = match mode {
                    Mode::Accept => !in_del,
                    Mode::Reject => !in_ins,
                };
                if visible {
                    for c in e.child_elems() {
                        match c.local() {
                            "t" | "delText" => out.push_str(&c.direct_text()),
                            "tab" => out.push('\t'),
                            "br" => out.push('\n'),
                            _ => {}
                        }
                    }
                }
            }
            l if is_transparent(l) => walk_text(&e.children, mode, in_ins, in_del, out),
            _ => {}
        }
    }
}

/// Visible text per paragraph after accepting or rejecting every revision.
pub fn visible_paragraphs(xml: &[u8], mode: Mode) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for span in docdiff::model::paragraph_spans(xml)? {
        let p = parse_paragraph(xml, span)?;
        let mark = p
            .find_child("pPr")
            .and_then(|ppr| ppr.find_child("rPr"))
            .map(|rpr| (rpr.find_child("ins").is_some(), rpr.find_child("del").is_some()))
            .unwrap_or((false, false));
        let dropped = match mode {
            Mode::Accept => mark.1,
            Mode::Reject => mark.0,
        };
        if dropped {
            continue;
        }
        let mut s = String::new();
        walk_text(&p.children, mode, false, false, &mut s);
        out.push(s);
    }
    Ok(out)
}

/// Fail unless accept-all gives B's paragraph texts and reject-all A's.
pub fn verify(xml: &[u8], diff: &DiffDoc, a: &[ParaModel], b: &[ParaModel]) -> Result<()> {
    let accept: Vec<String> = b.iter().map(|p| p.text.clone()).collect();
    for (mode, want) in [
        (Mode::Accept, accept),
        (Mode::Reject, expected_reject(diff, a, b)),
    ] {
        let got = visible_paragraphs(xml, mode)?;
        if got.len() != want.len() {
            bail!(
                "{mode:?}-all yields {} paragraphs, expected {}",
                got.len(),
                want.len()
            );
        }
        let same = |g: &str, w: &str| {
            if mode == Mode::Reject && diff.normalized {
                diff::para_key(g, true) == diff::para_key(w, true)
            } else {
                g == w
            }
        };
        if let Some((i, (g, w))) = got.iter().zip(&want).enumerate().find(|(_, (g, w))| !same(g, w)) {
            bail!("{mode:?}-all differs at paragraph {i}: got {g:?}, expected {w:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use docdiff::diff;
    use docdiff::model::parse_document;

    fn doc(paras: &[&str]) -> Vec<u8> {
        let body: String = paras
            .iter()
            .map(|t| {
                format!(
                    "<w:p><w:pPr><w:jc w:val=\"left\"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                    esc_text(t)
                )
            })
            .collect();
        format!("<w:document xmlns:w=\"urn:w\"><w:body>{body}</w:body></w:document>").into_bytes()
    }

    fn run(a: &[&str], b: &[&str]) -> (Vec<u8>, Parsed, Parsed, DiffDoc) {
        let (pa, pb) = (parse_document(doc(a)).unwrap(), parse_document(doc(b)).unwrap());
        let d = diff::compute("a", "b", &pa.paras, &pb.paras, false).unwrap();
        let opts = Opts {
            author: "T".into(),
            initials: None,
            email: None,
            date: "2026-01-01T00:00:00Z".into(),
        };
        let planned = plan(&d, &pb.paras, &[]).unwrap();
        let out = render_document(&pb, &pa, &d, &planned, &opts, &HashSet::new(), 100).unwrap();
        (out, pa, pb, d)
    }

    #[test]
    fn accept_gives_b_and_reject_gives_a() {
        let (out, pa, pb, d) = run(
            &["Keep this.", "I saw a participate there.", "Gone entirely paragraph here."],
            &["Keep this.", "I saw a participant there.", "Brand new tail paragraph."],
        );
        verify(&out, &d, &pa.paras, &pb.paras).unwrap();
        assert!(d.changes >= 2);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("<w:ins "));
        assert!(s.contains("<w:delText"));
    }

    #[test]
    fn deleted_and_inserted_paragraphs_carry_marks() {
        let (out, pa, pb, d) = run(&["one", "two gone completely different words"], &["one", "brand new unrelated content xyz"]);
        verify(&out, &d, &pa.paras, &pb.paras).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("<w:rPr><w:del "));
        assert!(s.contains("<w:rPr><w:ins "));
    }

    #[test]
    fn untouched_paragraphs_stay_byte_identical() {
        let (out, _, pb, _) = run(&["same one", "x y z q"], &["same one", "x y z r"]);
        let first = &pb.xml[pb.paras[0].span.0..pb.paras[0].span.1];
        let hay = String::from_utf8(out).unwrap();
        assert!(hay.contains(std::str::from_utf8(first).unwrap()));
    }

    #[test]
    fn revision_ids_are_unique() {
        let (out, ..) = run(&["a b c d e f g h"], &["a X c Y e Z g W"]);
        let s = String::from_utf8(out).unwrap();
        let ids: Vec<&str> = s.match_indices("w:id=\"").map(|(i, _)| &s[i + 6..i + 9]).collect();
        let set: HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), set.len());
    }

    #[test]
    fn hyperlink_stays_and_text_inside_is_diffed() {
        let mk = |u: &str| {
            format!(
                "<w:document xmlns:w=\"urn:w\"><w:body><w:p><w:r><w:t xml:space=\"preserve\">see </w:t></w:r><w:hyperlink r:id=\"rId9\"><w:r><w:t>{u}</w:t></w:r></w:hyperlink></w:p></w:body></w:document>"
            )
            .into_bytes()
        };
        let (pa, pb) = (parse_document(mk("MGXpsgsckbl")).unwrap(), parse_document(mk("MGXpsgsckbI")).unwrap());
        let d = diff::compute("a", "b", &pa.paras, &pb.paras, false).unwrap();
        let opts = Opts { author: "T".into(), initials: None, email: None, date: "d".into() };
        let planned = plan(&d, &pb.paras, &[]).unwrap();
        let out = render_document(&pb, &pa, &d, &planned, &opts, &HashSet::new(), 1).unwrap();
        verify(&out, &d, &pa.paras, &pb.paras).unwrap();
        let s = String::from_utf8(out).unwrap();
        let h0 = s.find("<w:hyperlink").unwrap();
        let h1 = s.find("</w:hyperlink>").unwrap();
        assert!(s[h0..h1].contains("<w:ins ") && s[h0..h1].contains("<w:del "));
        assert!(s[h0..].starts_with("<w:hyperlink r:id=\"rId9\">"));
    }
}
