//! Package-level writer: render a docdiff diff object as tracked changes in
//! B's `document.xml`, add the comment parts, and write B's package back
//! out with every other part copied byte-for-byte.

use crate::comments::{self, Note, RelBook, Resolved};
use crate::onto;
use crate::render::{self, Opts};
use anyhow::{Context, Result, anyhow, bail};
use docdiff::diff::DiffDoc;
use docdiff::import::{DOC_PART, load, read_part, require_part};
use docdiff::model::Parsed;
use docdiff::ooxml;
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const CT_PART: &str = "[Content_Types].xml";
const DOC_RELS: &str = "word/_rels/document.xml.rels";
const COMMENTS_CT: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml";
const COMMENTS_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";

/// ISO-8601 UTC timestamp for "now" (no chrono dependency).
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn style_ids(styles: Option<&[u8]>) -> HashSet<String> {
    let s = styles.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    s.match_indices("w:styleId=\"")
        .filter_map(|(i, _)| s[i + 11..].split('"').next().map(str::to_owned))
        .collect()
}

fn max_w_id(xml: &[u8]) -> u64 {
    let s = String::from_utf8_lossy(xml);
    s.match_indices(" w:id=\"")
        .filter_map(|(i, _)| s[i + 7..].split('"').next()?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

pub struct Summary {
    pub diff: DiffDoc,
    pub comments: usize,
    pub paragraphs_total: usize,
    pub paragraphs_changed: usize,
}

pub struct Request<'a> {
    /// The base (A) and revised (B) packages the diff was computed from.
    pub a: &'a Path,
    pub b: &'a Path,
    /// docdiff's diff object for A -> B (`docdiff A B --emit diff.json`).
    pub diff: DiffDoc,
    pub out: &'a Path,
    pub notes: Vec<Note>,
    pub opts: Opts,
    /// Build on A's package (our edits tracked in their document) instead
    /// of B's.
    pub onto_base: bool,
}

pub fn build(req: Request<'_>) -> Result<Summary> {
    let (pa, pb) = (load(req.a)?, load(req.b)?);
    let d = req.diff;
    let out = req.out;
    // the package the output is built on, the other side, and the diff
    // from the other side to the package
    let (pkg, pkg_doc, other_doc, rd) = if req.onto_base {
        onto::ensure_no_revisions(&pa.xml)?;
        (req.a, &pa, &pb, onto::reverse(&d))
    } else {
        (req.b, &pb, &pa, d.clone())
    };

    // existing comment part of the package (if any) decides id/rel numbering
    let rels_b = String::from_utf8_lossy(&read_part(pkg, DOC_RELS)?.unwrap_or_default()).into_owned();
    let existing_target = existing_comments_target(&rels_b);
    let comments_name = format!("word/{}", existing_target.as_deref().unwrap_or("comments.xml"));
    let comments_rels_name = format!(
        "word/_rels/{}.rels",
        existing_target.as_deref().unwrap_or("comments.xml")
    );
    let existing_comments = match &existing_target {
        Some(_) => read_part(pkg, &comments_name)?.map(|b| String::from_utf8_lossy(&b).into_owned()),
        None => None,
    };
    let first_id = existing_comments
        .as_deref()
        .and_then(comments::max_comment_id)
        .map_or(0, |m| m + 1);

    // notes always anchor to B's text (our revision)
    let resolved: Vec<Resolved> = comments::resolve(req.notes.clone(), first_id, &d, &pb.paras)?;
    let resolved = if req.onto_base { onto::translate(&d, resolved)? } else { resolved };
    let styles = style_ids(read_part(pkg, "word/styles.xml")?.as_deref());
    let first_rev = max_w_id(&pkg_doc.xml).max(
        resolved
            .last()
            .map_or(0, |r| u64::try_from(r.id).unwrap_or(0)),
    ) + 1;

    let planned = render::plan(&rd, &pkg_doc.paras, &resolved)?;
    let rendered = render::render_document(pkg_doc, other_doc, &rd, &planned, &req.opts, &styles, first_rev)?;
    render::verify(&rendered, &rd, &other_doc.paras, &pkg_doc.paras)
        .context("self-check failed: accept/reject does not reproduce the two documents")?;
    let new_doc = if req.onto_base {
        let swapped = onto::swap(&rendered)?;
        onto::verify_swap(&rendered, &swapped)?;
        swapped
    } else {
        rendered
    };

    // replacement / added parts
    let mut replaced: Vec<(String, Vec<u8>)> = vec![(DOC_PART.to_owned(), new_doc)];
    if !resolved.is_empty() {
        let comments_rels_existing = read_part(pkg, &comments_rels_name)?.map(|b| String::from_utf8_lossy(&b).into_owned());
        let mut rels = RelBook::new(comments_rels_existing.as_deref().map_or(0, comments::max_rid) + 1);
        let inner: String = resolved
            .iter()
            .map(|r| comments::comment_xml(r, &req.opts.author, req.opts.initials.as_deref(), &req.opts.date, &mut rels))
            .collect();
        let part = match existing_comments {
            Some(x) => insert_before_close(&x, "</w:comments>", &inner)?,
            None => comments::new_comments_part(&inner),
        };
        replaced.push((comments_name.clone(), part.into_bytes()));
        if !rels.entries.is_empty() {
            let rels_part = match comments_rels_existing {
                Some(x) => insert_before_close(&x, "</Relationships>", &rels.rels_xml_entries())?,
                None => format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{}</Relationships>",
                    rels.rels_xml_entries()
                ),
            };
            replaced.push((comments_rels_name.clone(), rels_part.into_bytes()));
        }
        if existing_target.is_none() {
            let new_rels = insert_before_close(
                &rels_b,
                "</Relationships>",
                &format!("<Relationship Id=\"rIdDdComments\" Type=\"{COMMENTS_REL}\" Target=\"comments.xml\"/>"),
            )?;
            replaced.push((DOC_RELS.to_owned(), new_rels.into_bytes()));
        }
        let ct = String::from_utf8_lossy(&require_part(pkg, CT_PART)?).into_owned();
        if !ct.contains(&format!("PartName=\"/{comments_name}\"")) {
            let new_ct = insert_before_close(
                &ct,
                "</Types>",
                &format!("<Override PartName=\"/{comments_name}\" ContentType=\"{COMMENTS_CT}\"/>"),
            )?;
            replaced.push((CT_PART.to_owned(), new_ct.into_bytes()));
        }
    }

    if let Some(email) = &req.opts.email {
        add_person(pkg, &mut replaced, &req.opts.author, email)?;
    }

    write_package(pkg, out, &replaced)?;
    Ok(summary(d, resolved.len(), pkg_doc))
}

fn summary(d: DiffDoc, comments: usize, pb: &Parsed) -> Summary {
    let changed = d
        .paragraphs
        .iter()
        .filter(|p| p.kind != docdiff::diff::ParaKind::Equal)
        .count();
    Summary {
        diff: d,
        comments,
        paragraphs_total: pb.paras.len(),
        paragraphs_changed: changed,
    }
}

fn existing_comments_target(rels: &str) -> Option<String> {
    rels.split("<Relationship ").skip(1).find_map(|r| {
        let tag = r.split('>').next()?;
        if !tag.contains("/relationships/comments\"") {
            return None;
        }
        let k = tag.find("Target=\"")? + 8;
        tag[k..].split('"').next().map(|t| t.trim_start_matches('/').trim_start_matches("word/").to_owned())
    })
}

fn insert_before_close(xml: &str, close: &str, inner: &str) -> Result<String> {
    if let Some(at) = xml.rfind(close) {
        return Ok(format!("{}{inner}{}", &xml[..at], &xml[at..]));
    }
    // empty root written as `<root .../>`: open it up
    let trimmed = xml.trim_end();
    let body = trimmed
        .strip_suffix("/>")
        .with_context(|| format!("no {close} and no self-closed root in part"))?;
    Ok(format!("{body}>{inner}{close}"))
}

fn write_package(src: &Path, out: &Path, replaced: &[(String, Vec<u8>)]) -> Result<()> {
    let mut zin = ZipArchive::new(File::open(src)?)?;
    let tmp = out.with_extension("docx.tmp");
    let mut zout = ZipWriter::new(File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut seen: HashSet<&str> = HashSet::new();
    for i in 0..zin.len() {
        let name = zin.by_index_raw(i)?.name().to_owned();
        match replaced.iter().find(|(n, _)| *n == name) {
            Some((n, bytes)) => {
                zout.start_file(n.as_str(), opts)?;
                zout.write_all(bytes)?;
                seen.insert(n.as_str());
            }
            None => zout.raw_copy_file(zin.by_index_raw(i)?)?,
        }
    }
    for (n, bytes) in replaced.iter().filter(|(n, _)| !seen.contains(n.as_str())) {
        zout.start_file(n.as_str(), opts)?;
        zout.write_all(bytes)?;
    }
    zout.finish()?;
    std::fs::rename(&tmp, out).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

// ----------------------------------------------------------- validation

/// Structural checks on a finished redline: well-formed XML parts,
/// unique revision ids, unique comment ids, and every comment range /
/// reference matched by a comment.
pub fn validate(path: &Path) -> Result<()> {
    let doc = require_part(path, DOC_PART)?;
    ooxml::parse_fragment(&doc).context("document.xml is not well-formed")?;
    let s = String::from_utf8_lossy(&doc).into_owned();

    let mut rev_ids: Vec<String> = Vec::new();
    for tag in ["<w:ins ", "<w:del "] {
        for (i, _) in s.match_indices(tag) {
            let end = s[i..].find('>').map_or(s.len(), |e| i + e);
            if let Some(k) = s[i..end].find("w:id=\"") {
                rev_ids.push(s[i + k + 6..].split('"').next().unwrap_or("").to_owned());
            }
        }
    }
    let uniq: HashSet<&String> = rev_ids.iter().collect();
    if uniq.len() != rev_ids.len() {
        bail!("duplicate revision ids in document.xml");
    }

    let ids_of = |tag: &str| -> Vec<String> {
        s.match_indices(tag)
            .filter_map(|(i, _)| {
                let end = s[i..].find('>')? + i;
                let k = s[i..end].find("w:id=\"")?;
                s[i + k + 6..].split('"').next().map(str::to_owned)
            })
            .collect()
    };
    let (starts, ends, refs) = (
        ids_of("<w:commentRangeStart "),
        ids_of("<w:commentRangeEnd "),
        ids_of("<w:commentReference "),
    );
    let mut cids: HashSet<String> = HashSet::new();
    if let Some(c) = read_part(path, "word/comments.xml")? {
        ooxml::parse_fragment(&c).context("comments.xml is not well-formed")?;
        let cs = String::from_utf8_lossy(&c).into_owned();
        for (i, _) in cs.match_indices("<w:comment ") {
            let end = cs[i..].find('>').map_or(cs.len(), |e| i + e);
            let k = cs[i..end].find("w:id=\"").context("comment without id")?;
            let id = cs[i + k + 6..].split('"').next().unwrap_or("").to_owned();
            if !cids.insert(id.clone()) {
                bail!("duplicate comment id {id}");
            }
        }
    }
    for (what, list) in [("start", &starts), ("end", &ends), ("reference", &refs)] {
        let set: HashSet<&String> = list.iter().collect();
        if set.len() != list.len() {
            bail!("duplicate comment {what} ids");
        }
        if let Some(bad) = list.iter().find(|id| !cids.contains(*id)) {
            bail!("comment {what} {bad} has no comment");
        }
    }
    if cids.len() != starts.len() && !starts.is_empty() {
        return Err(anyhow!(
            "{} comments but {} anchored ranges",
            cids.len(),
            starts.len()
        ));
    }
    // package integrity: every part still parses as a zip entry listing
    ZipArchive::new(File::open(path)?)?;
    Ok(())
}

/// A part as it will be written: the replaced version if any, else the package's.
fn current(pkg: &Path, replaced: &[(String, Vec<u8>)], name: &str) -> Result<Option<String>> {
    match replaced.iter().find(|(n, _)| n == name) {
        Some((_, b)) => Ok(Some(String::from_utf8_lossy(b).into_owned())),
        None => Ok(read_part(pkg, name)?.map(|b| String::from_utf8_lossy(&b).into_owned())),
    }
}

fn put(replaced: &mut Vec<(String, Vec<u8>)>, name: &str, body: String) {
    match replaced.iter_mut().find(|(n, _)| n == name) {
        Some(e) => e.1 = body.into_bytes(),
        None => replaced.push((name.to_owned(), body.into_bytes())),
    }
}

/// Record the author's email in `word/people.xml` (created and registered
/// if the package has none; an existing entry for the author is kept).
fn add_person(pkg: &Path, replaced: &mut Vec<(String, Vec<u8>)>, author: &str, email: &str) -> Result<()> {
    let person = comments::person_xml(author, email);
    let people = match current(pkg, replaced, comments::PEOPLE_PART)? {
        Some(x) if x.contains(&format!("w15:author=\"{}\"", ooxml::esc_attr(author))) => return Ok(()),
        Some(x) => insert_before_close(&x, "</w15:people>", &person)?,
        None => comments::new_people_part(&person),
    };
    put(replaced, comments::PEOPLE_PART, people);
    let rels = current(pkg, replaced, DOC_RELS)?.unwrap_or_default();
    if !rels.contains(comments::PEOPLE_REL) {
        let r = insert_before_close(
            &rels,
            "</Relationships>",
            &format!("<Relationship Id=\"rIdDdPeople\" Type=\"{}\" Target=\"people.xml\"/>", comments::PEOPLE_REL),
        )?;
        put(replaced, DOC_RELS, r);
    }
    let ct = current(pkg, replaced, CT_PART)?.context("package has no [Content_Types].xml")?;
    if !ct.contains("PartName=\"/word/people.xml\"") {
        let c = insert_before_close(
            &ct,
            "</Types>",
            &format!("<Override PartName=\"/word/people.xml\" ContentType=\"{}\"/>", comments::PEOPLE_CT),
        )?;
        put(replaced, CT_PART, c);
    }
    Ok(())
}
