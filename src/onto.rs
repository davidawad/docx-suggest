//! `--onto base`: build the suggestions copy on the BASE package instead of
//! the revised one. Used when the other side's .docx is the document of
//! record and our revision is a different format (e.g. .md): the output is
//! their file, layout untouched, with our edits as tracked changes.
//!
//! Method: reverse the diff (B -> A), render it on A's package with the
//! ordinary renderer (accept-all = A, reject-all = B, self-checked), then
//! swap every `w:ins` / `w:del` (and `w:t` / `w:delText` inside them), so
//! accept-all = B and reject-all = A. Comments are resolved against B, as
//! always, and translated into A's coordinates.

use crate::comments::{Resolved, Target};
use crate::render::{Mode, visible_paragraphs};
use anyhow::{Result, bail};
use docdiff::diff::{DiffDoc, OpKind, ParaDiff, ParaKind};
use docdiff::model::paragraph_spans;
use docdiff::ooxml::{self, Elem, Node, parse_fragment};

/// The same diff seen from the other side: A and B swapped, inserts and
/// deletes swapped, change numbers and run info kept.
pub fn reverse(d: &DiffDoc) -> DiffDoc {
    let flip = |k: OpKind| match k {
        OpKind::Insert => OpKind::Delete,
        OpKind::Delete => OpKind::Insert,
        OpKind::Equal => OpKind::Equal,
    };
    let paragraphs = d
        .paragraphs
        .iter()
        .map(|p| {
            let (a_para, b_para) = (p.b_para, p.a_para);
            ParaDiff {
                kind: match p.kind {
                    ParaKind::Insert => ParaKind::Delete,
                    ParaKind::Delete => ParaKind::Insert,
                    k => k,
                },
                a_para,
                b_para,
                ops: p
                    .ops
                    .iter()
                    .map(|o| docdiff::diff::Op {
                        kind: flip(o.kind),
                        para: b_para.or(a_para).unwrap_or(o.para),
                        a_start: o.b_start,
                        b_start: o.a_start,
                        text: o.text.clone(),
                        change: o.change,
                        runs: o.runs.clone(),
                    })
                    .collect(),
            }
        })
        .collect();
    DiffDoc {
        a: d.b.clone(),
        b: d.a.clone(),
        skipped_a: d.skipped_b,
        skipped_b: d.skipped_a,
        normalized: d.normalized,
        changes: d.changes,
        inserted_words: d.deleted_words,
        deleted_words: d.inserted_words,
        paragraphs,
    }
}

/// Map a char position of B paragraph text (forward diff) to A's text,
/// `None` when it falls inside text only B has.
fn map_pos(pd: &ParaDiff, pos: usize) -> Option<usize> {
    pd.ops.iter().filter(|o| o.kind != OpKind::Delete).find_map(|o| {
        let len = o.text.chars().count();
        (pos >= o.b_start && pos < o.b_start + len).then(|| match o.kind {
            OpKind::Equal => Some(o.a_start + (pos - o.b_start)),
            _ => None,
        })?
    })
}

/// Comment targets (resolved against B with the forward diff) in the
/// coordinates of A's package.
pub fn translate(d: &DiffDoc, notes: Vec<Resolved>) -> Result<Vec<Resolved>> {
    notes
        .into_iter()
        .map(|mut r| {
            r.target = match r.target {
                t @ (Target::Change(_) | Target::Changes(..)) => t,
                Target::Text { b_para, start, end } => {
                    let Some(pd) = d.paragraphs.iter().find(|p| p.b_para == Some(b_para)) else {
                        bail!("comment {}: B paragraph {b_para} is outside the diff (skipped?)", r.id);
                    };
                    let mapped = match (pd.kind, pd.a_para) {
                        (ParaKind::Equal, Some(a)) => Some((a, start, end)),
                        (ParaKind::Replace, Some(a)) => {
                            let inside_insert = pd.ops.iter().any(|o| {
                                o.kind == OpKind::Insert && o.b_start < end && o.b_start + o.text.chars().count() > start
                            });
                            match (inside_insert, map_pos(pd, start), end.checked_sub(1).and_then(|e| map_pos(pd, e))) {
                                (false, Some(s), Some(e)) => Some((a, s, e + 1)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    match mapped {
                        Some((a, s, e)) => Target::Text { b_para: a, start: s, end: e },
                        None => {
                            // the anchor touches our new text: span the changes it touches
                            let cs: Vec<usize> = pd
                                .ops
                                .iter()
                                .filter(|o| {
                                    let len = o.text.chars().count();
                                    match o.kind {
                                        OpKind::Insert => o.b_start < end && o.b_start + len > start,
                                        OpKind::Delete => o.b_start >= start && o.b_start <= end,
                                        OpKind::Equal => false,
                                    }
                                })
                                .filter_map(|o| o.change)
                                .collect();
                            match (cs.iter().min(), cs.iter().max()) {
                                (Some(lo), Some(hi)) => Target::Changes(*lo, *hi),
                                _ => bail!("comment {}: cannot place anchor in the base document", r.id),
                            }
                        }
                    }
                }
            };
            Ok(r)
        })
        .collect()
}

fn rename(e: &mut Elem, to: &str) {
    let rest = e.start[e.name.len()..].to_vec();
    e.name = to.to_owned();
    e.start = [to.as_bytes(), &rest].concat();
}

fn swap_nodes(nodes: &mut [Node], in_ins: bool, in_del: bool) {
    for n in nodes {
        let Node::Elem(e) = n else { continue };
        let (ins, del) = match e.local() {
            "ins" => {
                rename(e, "w:del");
                (false, true)
            }
            "del" => {
                rename(e, "w:ins");
                (true, false)
            }
            "t" if in_del => {
                rename(e, "w:delText");
                (in_ins, in_del)
            }
            "delText" if in_ins => {
                rename(e, "w:t");
                (in_ins, in_del)
            }
            "instrText" if in_del => {
                rename(e, "w:delInstrText");
                (in_ins, in_del)
            }
            "delInstrText" if in_ins => {
                rename(e, "w:instrText");
                (in_ins, in_del)
            }
            _ => (in_ins, in_del),
        };
        swap_nodes(&mut e.children, ins, del);
    }
}

/// Swap insertions and deletions in every paragraph that has either.
pub fn swap(xml: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(xml.len());
    let mut cursor = 0;
    for span in paragraph_spans(xml)? {
        let slice = &xml[span.0..span.1];
        out.extend_from_slice(&xml[cursor..span.0]);
        if slice.windows(7).any(|w| w == b"<w:ins " || w == b"<w:del ") {
            let mut nodes = parse_fragment(slice)?;
            swap_nodes(&mut nodes, false, false);
            out.extend(ooxml::to_bytes(&nodes));
        } else {
            out.extend_from_slice(slice);
        }
        cursor = span.1;
    }
    out.extend_from_slice(&xml[cursor..]);
    Ok(out)
}

/// The swapped document must accept to what the unswapped one rejected to,
/// and reject to what it accepted to (both already self-checked).
pub fn verify_swap(before: &[u8], after: &[u8]) -> Result<()> {
    if visible_paragraphs(after, Mode::Accept)? != visible_paragraphs(before, Mode::Reject)?
        || visible_paragraphs(after, Mode::Reject)? != visible_paragraphs(before, Mode::Accept)?
    {
        bail!("swap self-check failed: accept/reject not exchanged");
    }
    Ok(())
}

/// Base documents that already carry tracked changes would be swapped too.
pub fn ensure_no_revisions(xml: &[u8]) -> Result<()> {
    if xml.windows(7).any(|w| w == b"<w:ins " || w == b"<w:del ") {
        bail!("--onto base: the base document already has tracked changes; accept or reject them first");
    }
    Ok(())
}
