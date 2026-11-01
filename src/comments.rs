//! Comments: `notes.json` -> anchored `w:comment` entries whose bodies carry
//! real, clickable `w:hyperlink` elements (relationships live in
//! `word/_rels/comments.xml.rels`).

use docdiff::diff::DiffDoc;
use docdiff::model::ParaModel;
use docdiff::ooxml::{esc_attr, esc_text};
use anyhow::{Result, bail};
use serde::Deserialize;

const NS_W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const NS_R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub const REL_HYPERLINK: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AnchorSpec {
    /// 1-based change number in the diff object.
    Change(usize),
    /// Exact text of B (within one paragraph).
    Text(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    pub anchor: AnchorSpec,
    /// 1-based occurrence when the anchor text appears more than once.
    #[serde(default)]
    pub occurrence: Option<usize>,
    /// Restrict a text anchor to this B paragraph index (0-based).
    #[serde(default)]
    pub para: Option<usize>,
    pub body: String,
    #[serde(default)]
    pub links: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Target {
    Change(usize),
    /// Changes `lo..=hi`, all in one paragraph (one comment spanning them).
    Changes(usize, usize),
    /// Char range `[start, end)` of B paragraph `b_para`.
    Text { b_para: usize, start: usize, end: usize },
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub id: usize,
    pub note: Note,
    pub target: Target,
}

fn char_index(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

pub fn resolve(notes: Vec<Note>, first_id: usize, diff: &DiffDoc, b: &[ParaModel]) -> Result<Vec<Resolved>> {
    notes
        .into_iter()
        .enumerate()
        .map(|(i, note)| {
            let target = match &note.anchor {
                AnchorSpec::Change(c) => {
                    if *c == 0 || *c > diff.changes {
                        bail!(
                            "note {}: change #{c} out of range (diff has {} changes, numbered from 1)",
                            i + 1,
                            diff.changes
                        );
                    }
                    Target::Change(*c)
                }
                AnchorSpec::Text(t) => {
                    if t.is_empty() {
                        bail!("note {}: empty anchor text", i + 1);
                    }
                    let hits: Vec<(usize, usize)> = b
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| note.para.is_none_or(|p| p == *j))
                        .flat_map(|(j, p)| {
                            p.text
                                .match_indices(t.as_str())
                                .map(|(bi, _)| (j, char_index(&p.text, bi)))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    let pick = match (note.occurrence, hits.len()) {
                        (_, 0) => bail!("note {}: anchor text not found in B: {t:?}", i + 1),
                        (Some(n), len) if n >= 1 && n <= len => hits[n - 1],
                        (Some(n), len) => bail!("note {}: occurrence {n} requested, {len} found", i + 1),
                        (None, 1) => hits[0],
                        (None, len) => bail!(
                            "note {}: anchor text matches {len} places (paragraphs {:?}); set occurrence or para: {t:?}",
                            i + 1,
                            hits.iter().map(|h| h.0).collect::<Vec<_>>()
                        ),
                    };
                    Target::Text {
                        b_para: pick.0,
                        start: pick.1,
                        end: pick.1 + t.chars().count(),
                    }
                }
            };
            Ok(Resolved {
                id: first_id + i,
                note,
                target,
            })
        })
        .collect()
}

/// Split `body` into text and URL segments. Trailing sentence punctuation
/// is not part of the URL.
pub fn linkify(body: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    let mut rest = body;
    while let Some(pos) = ["https://", "http://"].iter().filter_map(|p| rest.find(p)).min() {
        let (before, tail) = rest.split_at(pos);
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\u{201d}' | '\u{200b}' | '\u{2060}'))
            .unwrap_or(tail.len());
        let mut url = &tail[..end];
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', ')', ']']);
        // keep a balanced closing paren that belongs to the URL
        if trimmed.len() != url.len() && url.ends_with(')') && trimmed.matches('(').count() > trimmed.matches(')').count() {
            url = &url[..trimmed.len() + 1];
        } else {
            url = trimmed;
        }
        if !before.is_empty() {
            out.push((false, before.to_owned()));
        }
        out.push((true, url.to_owned()));
        rest = &tail[url.len()..];
    }
    if !rest.is_empty() {
        out.push((false, rest.to_owned()));
    }
    out
}

pub fn initials(author: &str) -> String {
    let s: String = author
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    if s.is_empty() { "D".to_owned() } else { s }
}

/// Hands out one relationship id per distinct URL.
pub struct RelBook {
    next: u64,
    pub entries: Vec<(String, String)>,
}

impl RelBook {
    pub fn new(next: u64) -> Self {
        Self {
            next,
            entries: Vec::new(),
        }
    }

    fn id_for(&mut self, url: &str) -> String {
        if let Some((id, _)) = self.entries.iter().find(|(_, u)| u == url) {
            return id.clone();
        }
        let id = format!("rId{}", self.next);
        self.next += 1;
        self.entries.push((id.clone(), url.to_owned()));
        id
    }

    pub fn rels_xml_entries(&self) -> String {
        self.entries
            .iter()
            .map(|(id, url)| {
                format!(
                    "<Relationship Id=\"{id}\" Type=\"{REL_HYPERLINK}\" Target=\"{}\" TargetMode=\"External\"/>",
                    esc_attr(url)
                )
            })
            .collect()
    }
}

fn text_run(t: &str) -> String {
    format!("<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r>", esc_text(t))
}

fn link_run(rid: &str, url: &str) -> String {
    format!(
        "<w:hyperlink xmlns:r=\"{NS_R}\" r:id=\"{rid}\" w:history=\"1\"><w:r><w:rPr><w:color w:val=\"0563C1\"/><w:u w:val=\"single\"/></w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r></w:hyperlink>",
        esc_text(url)
    )
}

/// One `<w:comment>` element.
pub fn comment_xml(r: &Resolved, author: &str, initials_: Option<&str>, date: &str, rels: &mut RelBook) -> String {
    let paras: Vec<&str> = r.note.body.split('\n').collect();
    let last = paras.len().saturating_sub(1);
    let body_urls: Vec<String> = linkify(&r.note.body)
        .into_iter()
        .filter(|(l, _)| *l)
        .map(|(_, u)| u)
        .collect();
    let extra: Vec<&String> = r.note.links.iter().filter(|u| !body_urls.contains(u)).collect();
    let ps: String = paras
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let mut runs: String = linkify(line)
                .into_iter()
                .map(|(is_url, seg)| {
                    if is_url {
                        let rid = rels.id_for(&seg);
                        link_run(&rid, &seg)
                    } else {
                        text_run(&seg)
                    }
                })
                .collect();
            if i == last {
                for u in &extra {
                    runs.push_str(&text_run(" "));
                    runs.push_str(&link_run(&rels.id_for(u), u));
                }
            }
            let anno = if i == 0 { "<w:r><w:annotationRef/></w:r>" } else { "" };
            format!("<w:p>{anno}{runs}</w:p>")
        })
        .collect();
    format!(
        "<w:comment w:id=\"{}\" w:author=\"{}\" w:date=\"{}\" w:initials=\"{}\">{ps}</w:comment>",
        r.id,
        esc_attr(author),
        esc_attr(date),
        esc_attr(&initials_.map_or_else(|| initials(author), str::to_owned))
    )
}

pub fn new_comments_part(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:comments xmlns:w=\"{NS_W}\" xmlns:r=\"{NS_R}\">{inner}</w:comments>"
    )
}

/// Largest `w:id` among existing `<w:comment ...>` elements, if any.
pub fn max_comment_id(xml: &str) -> Option<usize> {
    xml.match_indices("<w:comment ")
        .filter_map(|(i, _)| {
            let tag_end = xml[i..].find('>')? + i;
            let tag = &xml[i..tag_end];
            let k = tag.find("w:id=\"")? + 6;
            tag[k..].split('"').next()?.parse::<usize>().ok()
        })
        .max()
}

/// Largest numeric suffix of `Id="rIdN"` in a relationships part.
pub fn max_rid(rels: &str) -> u64 {
    rels.match_indices("Id=\"rId")
        .filter_map(|(i, _)| rels[i + 7..].split('"').next()?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn linkify_splits_urls_and_trims_punctuation() {
        let segs = linkify("Source: https://www.law.cornell.edu/uscode/text/28/1746. Next, http://a.b/c)");
        let urls: Vec<_> = segs.iter().filter(|s| s.0).map(|s| s.1.as_str()).collect();
        assert_eq!(urls, vec!["https://www.law.cornell.edu/uscode/text/28/1746", "http://a.b/c"]);
        assert_eq!(segs.iter().map(|s| s.1.as_str()).collect::<String>(), "Source: https://www.law.cornell.edu/uscode/text/28/1746. Next, http://a.b/c)");
    }

    #[test]
    fn comment_has_real_hyperlink_and_rel() {
        let note = Note {
            anchor: AnchorSpec::Change(1),
            occurrence: None,
            para: None,
            body: "Why. See https://x.example/a".into(),
            links: vec!["https://y.example/b".into()],
        };
        let r = Resolved { id: 3, note, target: Target::Change(1) };
        let mut rels = RelBook::new(1);
        let xml = comment_xml(&r, "Test Author", None, "2026-01-01T00:00:00Z", &mut rels);
        assert_eq!(xml.matches("<w:hyperlink ").count(), 2);
        assert_eq!(rels.entries.len(), 2);
        assert!(xml.contains("w:initials=\"DA\""));
        assert!(rels.rels_xml_entries().contains("TargetMode=\"External\""));
    }

    #[test]
    fn id_scanners() {
        assert_eq!(max_comment_id("<w:comments><w:comment w:id=\"4\" w:author=\"x\"/><w:comment w:id=\"9\"/></w:comments>"), Some(9));
        assert_eq!(max_comment_id("<w:comments/>"), None);
        assert_eq!(max_rid("<R Id=\"rId3\"/><R Id=\"rIdFooter\"/><R Id=\"rId12\"/>"), 12);
    }
}

pub const PEOPLE_PART: &str = "word/people.xml";
pub const PEOPLE_REL: &str = "http://schemas.microsoft.com/office/2011/relationships/people";
pub const PEOPLE_CT: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.people+xml";
const NS_W15: &str = "http://schemas.microsoft.com/office/word/2012/wordml";

/// One `w15:person` entry binding the author name to an email identity.
pub fn person_xml(author: &str, email: &str) -> String {
    format!(
        "<w15:person w15:author=\"{}\"><w15:presenceInfo w15:providerId=\"None\" w15:userId=\"{}\"/></w15:person>",
        esc_attr(author),
        esc_attr(email)
    )
}

pub fn new_people_part(inner: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w15:people xmlns:w15=\"{NS_W15}\">{inner}</w15:people>"
    )
}
