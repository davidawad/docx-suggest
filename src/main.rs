//! docx-suggest — turn a docdiff diff object into a suggestions .docx.
//!
//! docdiff computes the difference between a base (A) and a revised (B)
//! document and nothing else. This tool composes it: it takes docdiff's
//! diff object (`--diff diff.json`, or runs `docdiff A B --emit -` itself)
//! and writes B's own package with every difference shown as a tracked
//! change (`w:ins` / `w:del`) plus optional comments whose links are
//! clickable. The reader accepts or rejects each change; accept-all gives
//! B and reject-all gives A (checked before the file is written).

use anyhow::{Context, Result, bail};
use clap::Parser;
use docdiff::diff::DiffDoc;
use docdiff::import;
use std::path::{Path, PathBuf};
use std::process::Command;

mod comments;
mod onto;
mod package;
mod render;

#[derive(Parser, Debug)]
#[command(name = "docx-suggest", version)]
#[command(about = "Render a docdiff diff object as a suggestions .docx (tracked changes + linked comments).")]
struct Cli {
    /// Base document (A): what the other side wrote.
    base: PathBuf,

    /// Revised document (B): our version. The output is built on its package.
    revised: PathBuf,

    /// Output .docx.
    #[arg(short, long, value_name = "OUT.docx")]
    out: PathBuf,

    /// docdiff's diff object for BASE -> REVISED. Omitted: run
    /// `docdiff BASE REVISED --emit -` (DOCDIFF_BIN or docdiff on PATH),
    /// passing --reference-doc and anything after `--`.
    #[arg(long, value_name = "diff.json")]
    diff: Option<PathBuf>,

    /// Comments: JSON list of {anchor: change number | exact text, body,
    /// links[], occurrence?, para?}. URLs in a body become hyperlinks.
    #[arg(long, value_name = "notes.json")]
    comments: Option<PathBuf>,

    /// Revision and comment author.
    #[arg(long, value_name = "NAME", env = "DOCX_SUGGEST_AUTHOR", default_value = "docx-suggest")]
    author: String,

    /// Author email, recorded in word/people.xml (the author identity Word
    /// and ONLYOFFICE show). Omitted: no people part is written.
    #[arg(long, value_name = "EMAIL", env = "DOCX_SUGGEST_EMAIL")]
    email: Option<String>,

    /// Comment initials (default: first letters of --author).
    #[arg(long, value_name = "XY", env = "DOCX_SUGGEST_INITIALS")]
    initials: Option<String>,

    /// Revision and comment date (ISO 8601); default now.
    #[arg(long, value_name = "ISO")]
    date: Option<String>,

    /// Which package the output is built on: `revised` (default; its layout)
    /// or `base` (the other side's file stays the document, with our edits
    /// tracked in it: use when REVISED is .md / .org and BASE is their .docx).
    #[arg(long, value_enum, default_value_t = Onto::Revised)]
    onto: Onto,

    /// pandoc --reference-doc for .md / .org inputs (same one docdiff used).
    #[arg(long, value_name = "REF.docx")]
    reference_doc: Option<PathBuf>,

    /// Extra docdiff options when docdiff is run here (e.g. --normalize,
    /// --skip-a 0..3).
    #[arg(last = true)]
    docdiff_args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Onto {
    Revised,
    Base,
}

fn run_docdiff(cli: &Cli) -> Result<DiffDoc> {
    let bin = std::env::var_os("DOCDIFF_BIN").map_or_else(|| PathBuf::from("docdiff"), PathBuf::from);
    let mut cmd = Command::new(&bin);
    cmd.arg(&cli.base).arg(&cli.revised).args(["--emit", "-"]);
    if let Some(r) = &cli.reference_doc {
        cmd.arg("--reference-doc").arg(r);
    }
    cmd.args(&cli.docdiff_args);
    let res = cmd
        .output()
        .with_context(|| format!("running {} (set DOCDIFF_BIN or pass --diff)", bin.display()))?;
    if !res.status.success() {
        bail!("docdiff failed: {}", String::from_utf8_lossy(&res.stderr).trim());
    }
    eprint!("{}", String::from_utf8_lossy(&res.stderr));
    serde_json::from_slice(&res.stdout).context("parsing docdiff's diff object")
}

fn read_diff(path: &Path) -> Result<DiffDoc> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("{} is not a docdiff diff object", path.display()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.diff.is_some() && !cli.docdiff_args.is_empty() {
        bail!("docdiff options after `--` only apply when docdiff runs here; drop --diff or the options");
    }
    let diff = match &cli.diff {
        Some(p) => read_diff(p)?,
        None => run_docdiff(&cli)?,
    };
    let notes: Vec<comments::Note> = match &cli.comments {
        Some(p) => serde_json::from_slice(&std::fs::read(p).with_context(|| format!("reading {}", p.display()))?)
            .with_context(|| format!("parsing {} (expected a JSON list of notes)", p.display()))?,
        None => Vec::new(),
    };
    let scratch = cli
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (a, a_tmp) = import::ensure_docx(&cli.base, cli.reference_doc.as_deref(), scratch)?;
    let (b, b_tmp) = import::ensure_docx(&cli.revised, cli.reference_doc.as_deref(), scratch)?;
    let result = package::build(package::Request {
        a: &a,
        b: &b,
        diff,
        out: &cli.out,
        notes,
        opts: render::Opts {
            author: cli.author.clone(),
            initials: cli.initials.clone(),
            email: cli.email.clone(),
            date: cli.date.clone().unwrap_or_else(package::now_iso),
        },
        onto_base: cli.onto == Onto::Base,
    })
    .and_then(|s| package::validate(&cli.out).map(|()| s));
    for (tmp, made) in [(&a, a_tmp), (&b, b_tmp)] {
        if made {
            let _ = std::fs::remove_file(tmp);
        }
    }
    let s = result?;
    eprintln!(
        "docx-suggest: {} changes in {}/{} paragraphs; {} comments -> {}",
        s.diff.changes,
        s.paragraphs_changed,
        s.paragraphs_total,
        s.comments,
        cli.out.display()
    );
    Ok(())
}
