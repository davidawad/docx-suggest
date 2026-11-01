//! End-to-end tests for `docx-suggest` (composing `docdiff --emit`) on
//! synthetic packages. They run the `docdiff` binary named by `DOCDIFF_BIN`
//! (default: `docdiff` on PATH). The `#[ignore]`d test reads your own
//! documents from the environment:
//!
//!   DOCX_SUGGEST_REAL_A, DOCX_SUGGEST_REAL_B  base and revised .docx
//!   DOCX_SUGGEST_REAL_NOTES                   notes.json (optional)
//!   DOCX_SUGGEST_REAL_CHANGES / _COMMENTS     expected counts (optional)
//!
//! run with `cargo test --test suggest -- --ignored`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

fn para(text: &str, rpr: &str) -> String {
    format!(
        "<w:p><w:pPr><w:jc w:val=\"left\"/></w:pPr><w:r>{rpr}<w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>"
    )
}

/// A small but structurally realistic package: styles, a footer with a VML
/// text box, numbering, a hyperlink paragraph and a field paragraph.
fn package(paras: &[&str], link_text: &str) -> Vec<u8> {
    let body: String = paras
        .iter()
        .map(|t| para(t, "<w:rPr><w:b/></w:rPr>"))
        .collect();
    let link = format!(
        "<w:p><w:r><w:t xml:space=\"preserve\">See </w:t></w:r><w:hyperlink r:id=\"rId9\"><w:r><w:t>{link_text}</w:t></w:r></w:hyperlink></w:p>"
    );
    let field = "<w:p><w:r><w:fldChar w:fldCharType=\"begin\"/></w:r><w:r><w:instrText xml:space=\"preserve\"> PAGE </w:instrText></w:r><w:r><w:fldChar w:fldCharType=\"separate\"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType=\"end\"/></w:r></w:p>";
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<w:document xmlns:w=\"{W}\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><w:body>{body}{link}{field}<w:sectPr><w:footerReference w:type=\"default\" r:id=\"rId8\"/></w:sectPr></w:body></w:document>"
    );
    let parts: Vec<(&str, String)> = vec![
        (
            "[Content_Types].xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"xml\" ContentType=\"application/xml\"/><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>".to_owned(),
        ),
        (
            "_rels/.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>".to_owned(),
        ),
        ("word/document.xml", document),
        (
            "word/_rels/document.xml.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId8\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer\" Target=\"footer1.xml\"/><Relationship Id=\"rId9\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" Target=\"https://example.com/\" TargetMode=\"External\"/></Relationships>".to_owned(),
        ),
        (
            "word/styles.xml",
            format!("<w:styles xmlns:w=\"{W}\"><w:style w:styleId=\"Normal\"/></w:styles>"),
        ),
        (
            "word/numbering.xml",
            format!("<w:numbering xmlns:w=\"{W}\"/>"),
        ),
        (
            "word/footer1.xml",
            format!("<w:ftr xmlns:w=\"{W}\" xmlns:v=\"urn:schemas-microsoft-com:vml\"><w:p><w:r><w:pict><v:shape><v:textbox><w:txbxContent><w:p><w:r><w:t>1 2 3 line numbers</w:t></w:r></w:p></w:txbxContent></v:textbox></v:shape></w:pict></w:r></w:p></w:ftr>"),
        ),
    ];
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in parts {
        z.start_file(name, SimpleFileOptions::default()).unwrap();
        z.write_all(content.as_bytes()).unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn parts(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut z = ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut v = Vec::new();
            f.read_to_end(&mut v).unwrap();
            (f.name().to_owned(), v)
        })
        .collect()
}

fn text(parts: &BTreeMap<String, Vec<u8>>, name: &str) -> String {
    String::from_utf8(parts[name].clone()).unwrap()
}

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        let p = base.join(format!("docx-suggest-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn docdiff_bin() -> PathBuf {
    std::env::var_os("DOCDIFF_BIN").map_or_else(|| PathBuf::from("docdiff"), PathBuf::from)
}

fn docdiff(args: &[&str]) -> std::process::Output {
    Command::new(docdiff_bin()).args(args).output().unwrap()
}

/// `docx-suggest A B -o OUT [opts] [-- docdiff opts]`, docdiff run inside.
fn suggest(a: &Path, b: &Path, out: &Path, opts: &[&str], docdiff_opts: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_docx-suggest"));
    c.env("DOCDIFF_BIN", docdiff_bin()).arg(a).arg(b).arg("-o").arg(out).args(opts);
    if !docdiff_opts.is_empty() {
        c.arg("--").args(docdiff_opts);
    }
    c.output().unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn base_and_revised(d: &Dir) -> (PathBuf, PathBuf) {
    let a = d.write(
        "a.docx",
        &package(
            &["Keep this paragraph.", "I saw a participate there today.", "This one goes away entirely, sadly."],
            "MGXpsgsckbl",
        ),
    );
    let b = d.write(
        "b.docx",
        &package(
            &["Keep this paragraph.", "I saw a participant there today.", "A brand new paragraph appears here."],
            "MGXpsgsckbI",
        ),
    );
    (a, b)
}

fn revision_ids(xml: &str) -> Vec<String> {
    ["<w:ins ", "<w:del "]
        .iter()
        .flat_map(|tag| {
            xml.match_indices(tag).filter_map(|(i, _)| {
                let end = xml[i..].find('>')? + i;
                let k = xml[i..end].find("w:id=\"")?;
                xml[i + k + 6..].split('"').next().map(str::to_owned)
            })
        })
        .collect()
}

#[test]
fn accept_all_is_b_and_only_document_and_comment_parts_change() {
    let d = Dir::new("pkg");
    let (a, b) = base_and_revised(&d);
    let out = d.0.join("out.docx");
    let r = suggest(&a, &b, &out, &["--author", "Test Author", "--date", "2026-10-31T00:00:00Z"], &[]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));

    let (pb, po) = (parts(&b), parts(&out));
    let changed: Vec<&String> = pb.keys().filter(|k| pb[*k] != po[*k]).collect();
    assert_eq!(changed, vec!["word/document.xml"], "only document.xml may change without comments");
    assert_eq!(pb.keys().collect::<Vec<_>>(), po.keys().collect::<Vec<_>>());

    // accept-all, read with docdiff's own extractor: identical text to B
    let diff = docdiff(&["--patch", "--no-cache", s(&out), s(&b)]);
    assert!(diff.stdout.is_empty(), "accept-all text differs from B:\n{}", String::from_utf8_lossy(&diff.stdout));

    let xml = text(&po, "word/document.xml");
    assert!(xml.contains("<w:ins ") && xml.contains("<w:delText"));
    // hyperlink and field structure untouched
    assert!(xml.contains("<w:hyperlink r:id=\"rId9\">"));
    assert!(xml.contains("<w:fldChar w:fldCharType=\"separate\"/>"));
    assert!(xml.contains("<w:sectPr><w:footerReference w:type=\"default\" r:id=\"rId8\"/></w:sectPr>"));
    let ids = revision_ids(&xml);
    assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len(), "revision ids must be unique");
}

#[test]
fn diff_json_has_word_level_ops_with_run_info() {
    let d = Dir::new("json");
    let (a, b) = base_and_revised(&d);
    let emit = d.0.join("diff.json");
    let r = docdiff(&[s(&a), s(&b), "--emit", s(&emit)]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&emit).unwrap()).unwrap();
    assert!(v["changes"].as_u64().unwrap() >= 3);
    let ops: Vec<&serde_json::Value> = v["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["ops"].as_array().unwrap())
        .collect();
    assert!(ops.iter().any(|o| o["kind"] == "delete" && o["text"] == "participate"));
    let ins = ops.iter().find(|o| o["kind"] == "insert" && o["text"] == "participant").unwrap();
    assert!(ins["runs"][0]["rpr"].as_str().unwrap().contains("<w:b/>"), "insert carries B's run properties");
    assert!(ins["para"].is_u64());
}

#[test]
fn comments_are_anchored_and_links_are_clickable() {
    let d = Dir::new("comments");
    let (a, b) = base_and_revised(&d);
    let notes = d.write(
        "notes.json",
        br#"[
          {"anchor": 1, "body": "Typo. See https://example.org/src.", "links": ["https://example.net/extra"]},
          {"anchor": "Keep this paragraph.", "body": "Unchanged text, annotated."}
        ]"#,
    );
    let out = d.0.join("out.docx");
    let r = suggest(&a, &b, &out, &["--author", "Test Author", "--comments", s(&notes)], &[]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let po = parts(&out);
    let pb = parts(&b);

    let comments = text(&po, "word/comments.xml");
    assert_eq!(comments.matches("<w:comment ").count(), 2);
    assert_eq!(comments.matches("<w:hyperlink ").count(), 2);
    assert!(comments.contains("w:author=\"Test Author\""));
    let rels = text(&po, "word/_rels/comments.xml.rels");
    assert!(rels.contains("Target=\"https://example.org/src\"") || rels.contains("Target=\"https://example.org/src.\""));
    assert!(rels.contains("Target=\"https://example.net/extra\""));
    assert!(rels.contains("TargetMode=\"External\""));
    assert!(text(&po, "[Content_Types].xml").contains("/word/comments.xml"));
    assert!(text(&po, "word/_rels/document.xml.rels").contains("relationships/comments"));

    let xml = text(&po, "word/document.xml");
    for id in ["0", "1"] {
        assert_eq!(xml.matches(&format!("<w:commentRangeStart w:id=\"{id}\"/>")).count(), 1);
        assert_eq!(xml.matches(&format!("<w:commentRangeEnd w:id=\"{id}\"/>")).count(), 1);
        assert_eq!(xml.matches(&format!("<w:commentReference w:id=\"{id}\"/>")).count(), 1);
    }
    // everything except the document and the comment plumbing is B, untouched
    let touched = [
        "word/document.xml",
        "word/comments.xml",
        "word/_rels/comments.xml.rels",
        "word/_rels/document.xml.rels",
        "[Content_Types].xml",
    ];
    for (name, bytes) in &pb {
        if !touched.contains(&name.as_str()) {
            assert_eq!(&po[name], bytes, "{name} must be byte-identical");
        }
    }
    // text is still B's with the notes in place
    let diff = docdiff(&["--patch", "--no-cache", s(&out), s(&b)]);
    assert!(diff.stdout.is_empty());
}

#[test]
fn ambiguous_or_missing_anchor_text_is_an_error() {
    let d = Dir::new("anchors");
    let (a, b) = base_and_revised(&d);
    let out = d.0.join("out.docx");
    for (notes, needle) in [
        (r#"[{"anchor": "paragraph", "body": "x"}]"#, "matches"),
        (r#"[{"anchor": "no such text", "body": "x"}]"#, "not found"),
        (r#"[{"anchor": 99, "body": "x"}]"#, "out of range"),
    ] {
        let n = d.write("n.json", notes.as_bytes());
        let r = suggest(&a, &b, &out, &["--comments", s(&n)], &[]);
        assert!(!r.status.success());
        let err = String::from_utf8_lossy(&r.stderr);
        assert!(err.contains(needle), "{err}");
    }
}

#[test]
fn skipped_regions_are_left_untouched_and_still_verify() {
    let d = Dir::new("skip");
    let a = d.write("a.docx", &package(&["TOC old 1", "TOC old 2", "body same", "tail one"], "x"));
    let b = d.write("b.docx", &package(&["TOC new entirely different", "body same", "tail two"], "x"));
    let out = d.0.join("out.docx");
    let r = suggest(&a, &b, &out, &[], &["--skip-a", "0..2", "--skip-b", "0..1"]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let xml = text(&parts(&out), "word/document.xml");
    assert!(xml.contains("TOC new entirely different"));
    assert!(!xml.contains("TOC old"));
}

#[test]
fn markdown_revised_goes_through_pandoc() {
    if Command::new("pandoc").arg("--version").output().is_err() {
        eprintln!("pandoc missing; skipping");
        return;
    }
    let d = Dir::new("md");
    let a = d.write("a.md", b"# Title\n\nOne two three.\n");
    let b = d.write("b.md", b"# Title\n\nOne TWO three.\n");
    let out = d.0.join("out.docx");
    let r = suggest(&a, &b, &out, &[], &[]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let xml = text(&parts(&out), "word/document.xml");
    assert!(xml.contains("<w:ins ") && xml.contains("<w:del "));
    // the pandoc scratch files are cleaned up
    assert!(std::fs::read_dir(&d.0).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".docdiff-")));
}

#[test]
#[ignore = "needs real documents: set DOCX_SUGGEST_REAL_A / _B"]
fn real_documents_roundtrip() {
    let (Some(a), Some(b)) = (
        std::env::var_os("DOCX_SUGGEST_REAL_A"),
        std::env::var_os("DOCX_SUGGEST_REAL_B"),
    ) else {
        panic!("DOCX_SUGGEST_REAL_A / DOCX_SUGGEST_REAL_B not set");
    };
    let (a, b) = (PathBuf::from(a), PathBuf::from(b));
    let d = Dir::new("real");
    let out = d.0.join("out.docx");
    let notes = std::env::var_os("DOCX_SUGGEST_REAL_NOTES").map(PathBuf::from);
    let opts: Vec<&str> = notes.as_deref().map_or_else(Vec::new, |n| vec!["--comments", s(n)]);
    let r = suggest(&a, &b, &out, &opts, &[]);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let diff = docdiff(&["--patch", "--no-cache", s(&out), s(&b)]);
    assert!(diff.stdout.is_empty(), "accept-all differs from B");
    let err = String::from_utf8_lossy(&r.stderr);
    if let Ok(n) = std::env::var("DOCX_SUGGEST_REAL_CHANGES") {
        assert!(err.contains(&format!("{n} changes")), "{err}");
    }
    if let Ok(n) = std::env::var("DOCX_SUGGEST_REAL_COMMENTS") {
        assert!(err.contains(&format!("{n} comments")), "{err}");
    }
}

fn sample(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/sample").join(name)
}

#[test]
fn checked_in_sample_renders_onto_revised_and_onto_base() {
    if Command::new("pandoc").arg("--version").output().is_err() {
        eprintln!("pandoc missing; skipping");
        return;
    }
    let notes = sample("notes.json");
    for onto in ["revised", "base"] {
        let d = Dir::new(onto);
        let out = d.0.join("out.docx");
        let r = suggest(
            &sample("base.docx"),
            &sample("revised.md"),
            &out,
            &["--onto", onto, "--comments", s(&notes), "--author", "Sample Reviewer"],
            &[],
        );
        assert!(r.status.success(), "{onto}: {}", String::from_utf8_lossy(&r.stderr));
        let p = parts(&out);
        let xml = text(&p, "word/document.xml");
        assert!(xml.contains("<w:ins ") && xml.contains("<w:del "), "{onto}");
        let comments = text(&p, "word/comments.xml");
        assert_eq!(comments.matches("<w:comment ").count(), 3, "{onto}");
        assert!(text(&p, "word/_rels/comments.xml.rels").contains("https://example.com/payment-terms"));
    }
}
