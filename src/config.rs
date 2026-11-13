//! Optional user config: `$XDG_CONFIG_HOME/docx-suggest/config.toml` (else
//! `~/.config/docx-suggest/config.toml`).
//!
//! When it defines `[[authors]]`, that list is a whitelist: only a listed
//! identity can be recorded on the output, the first entry is the default,
//! and there is deliberately no flag or environment variable to get around
//! it. With no file, or a file without `authors`, any `--author` is accepted.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;

const UNCONFIGURED_AUTHOR: &str = "docx-suggest";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub authors: Option<Vec<Author>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub name: String,
    pub email: String,
    pub initials: Option<String>,
}

/// The identity recorded on revisions and comments.
#[derive(Debug, PartialEq, Eq)]
pub struct Identity {
    pub author: String,
    pub email: Option<String>,
    pub initials: Option<String>,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("docx-suggest").join("config.toml"))
}

/// Reads the config. A file that exists but cannot be read or parsed is an
/// error, never "no whitelist".
pub fn load() -> Result<Option<Config>> {
    let Some(path) = path() else { return Ok(None) };
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Applies the whitelist, if any, to the requested identity.
pub fn resolve(
    config: Option<&Config>,
    author: Option<String>,
    email: Option<String>,
    initials: Option<String>,
) -> Result<Identity> {
    let Some(list) = config.and_then(|c| c.authors.as_deref()) else {
        return Ok(Identity {
            author: author.unwrap_or_else(|| UNCONFIGURED_AUTHOR.to_owned()),
            email,
            initials,
        });
    };
    let allowed = || {
        list.iter()
            .map(|a| format!("{} <{}>", a.name, a.email))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(first) = list.first() else {
        bail!("the config's authors list is empty, so no author is allowed");
    };
    let entry = match (&author, &email) {
        (Some(wanted), _) => list.iter().find(|a| a.name == *wanted || a.email == *wanted),
        (None, Some(wanted)) => list.iter().find(|a| a.email == *wanted),
        (None, None) => Some(first),
    };
    let Some(entry) = entry else {
        let wanted = author.or(email).unwrap_or_default();
        bail!("author {wanted:?} is not in the config's authors list; allowed: {}", allowed());
    };
    if let Some(wanted) = email.filter(|e| *e != entry.email) {
        bail!("email {wanted:?} does not match {} <{}>; allowed: {}", entry.name, entry.email, allowed());
    }
    Ok(Identity {
        author: entry.name.clone(),
        email: Some(entry.email.clone()),
        initials: initials.or_else(|| entry.initials.clone()),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            "[[authors]]\nname = \"A One\"\nemail = \"a@x.test\"\ninitials = \"AO\"\n\n\
             [[authors]]\nname = \"B Two\"\nemail = \"b@x.test\"\n",
        )
        .unwrap()
    }

    fn s(v: &str) -> Option<String> {
        Some(v.to_owned())
    }

    #[test]
    fn no_config_or_no_authors_accepts_anything() {
        let got = resolve(None, s("Anyone"), None, None).unwrap();
        assert_eq!(got.author, "Anyone");
        assert_eq!(resolve(None, None, None, None).unwrap().author, UNCONFIGURED_AUTHOR);
        let empty: Config = toml::from_str("").unwrap();
        assert_eq!(resolve(Some(&empty), s("Anyone"), None, None).unwrap().author, "Anyone");
    }

    #[test]
    fn first_author_is_the_default_with_its_email_and_initials() {
        let got = resolve(Some(&config()), None, None, None).unwrap();
        assert_eq!(
            got,
            Identity { author: "A One".into(), email: s("a@x.test"), initials: s("AO") }
        );
    }

    #[test]
    fn listed_author_matches_by_name_or_email() {
        let c = config();
        assert_eq!(resolve(Some(&c), s("B Two"), None, None).unwrap().email, s("b@x.test"));
        assert_eq!(resolve(Some(&c), s("b@x.test"), None, None).unwrap().author, "B Two");
        assert_eq!(resolve(Some(&c), None, s("b@x.test"), None).unwrap().author, "B Two");
    }

    #[test]
    fn unlisted_author_or_email_is_refused() {
        let c = config();
        assert!(resolve(Some(&c), s("Claude"), None, None).is_err());
        assert!(resolve(Some(&c), None, s("c@x.test"), None).is_err());
        assert!(resolve(Some(&c), s("A One"), s("b@x.test"), None).is_err());
    }

    #[test]
    fn empty_list_allows_nobody() {
        let c: Config = toml::from_str("authors = []").unwrap();
        assert!(resolve(Some(&c), None, None, None).is_err());
    }
}
