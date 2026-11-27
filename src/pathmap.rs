//! Where a Claude Code session from another device goes on this one. Claude Code keeps
//! sessions in one folder per working directory, named after the directory's path, so a session
//! made under `C:\Users\a\work\x` would land in a folder nobody on a Mac ever opens. A working
//! directory under the source device's home moves under this device's home, the same way
//! relative to it; `[[paths.map]]` rules in config.toml come first. Only the folder name is
//! worked out here: the session's content, paths inside it included, is never touched.
//!
//! Paths from other devices are plain strings in that device's style (`/` or `C:\`), so
//! nothing here goes through `std::path`, which only knows this system's style.
//!
//! The trusted source can send rules too (`merge`). Per `from`, the device's own rule applies
//! first; once the source changes its rule for that `from`, the source's applies, until the
//! device's own changes again; a rule the device locked always applies. A source rule with an
//! empty `to` drops the device's own rule. Which side changed last is told apart by what each
//! side said the time before (`Memory`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::PathMap;

/// Claude Code's folder name for a working directory: every UTF-16 unit that is not an ASCII
/// letter or digit becomes `-`; past 200 characters it is cut and a hash of the whole path is
/// added (read from Claude Code 2.1.327).
pub fn claude_project_dir(path: &str) -> String {
    const MAX: usize = 200;
    let units: Vec<u16> = path.encode_utf16().collect();
    let mut name: String = units
        .iter()
        .map(|&u| match u8::try_from(u) {
            Ok(b) if b.is_ascii_alphanumeric() => b as char,
            _ => '-',
        })
        .collect();
    if name.len() <= MAX {
        return name;
    }
    let mut hash: i32 = 0;
    for &u in &units {
        hash = hash.wrapping_shl(5).wrapping_sub(hash).wrapping_add(i32::from(u));
    }
    name.truncate(MAX);
    format!("{name}-{}", base36(i64::from(hash).unsigned_abs()))
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Unix,
    Windows,
}

/// A path from any device, split into its parts.
#[derive(Debug)]
struct Foreign {
    style: Style,
    parts: Vec<String>,
}

impl Foreign {
    fn parse(path: &str) -> Option<Foreign> {
        let bytes = path.as_bytes();
        let style = if path.starts_with('/') {
            Style::Unix
        } else if (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':') || path.starts_with(r"\\") {
            Style::Windows
        } else {
            return None;
        };
        let parts = path
            .split(|c| c == '/' || (style == Style::Windows && c == '\\'))
            .filter(|p| !p.is_empty() && *p != ".")
            .map(str::to_owned)
            .collect();
        Some(Foreign { style, parts })
    }

    /// The parts after `prefix`, if this path is `prefix` or under it.
    fn under<'a>(&'a self, prefix: &Foreign) -> Option<&'a [String]> {
        if self.style != prefix.style || self.parts.len() < prefix.parts.len() {
            return None;
        }
        let same = |a: &String, b: &String| match self.style {
            Style::Unix => a == b,
            Style::Windows => a.eq_ignore_ascii_case(b),
        };
        let matches = self.parts.iter().zip(&prefix.parts).all(|(a, b)| same(a, b));
        matches.then(|| &self.parts[prefix.parts.len()..])
    }
}

/// The home folder in the usual layouts: `/Users/<name>`, `/home/<name>`, `/root`,
/// `C:\Users\<name>`. What device files say comes first; this is for devices registered
/// before they said it.
fn usual_home(path: &Foreign) -> Option<Foreign> {
    let parts = &path.parts;
    let keep = match path.style {
        Style::Unix => match parts.first().map(String::as_str) {
            Some("Users" | "home") if parts.len() >= 2 => 2,
            Some("root") => 1,
            _ => return None,
        },
        Style::Windows => {
            let drive = parts.first()?;
            let is_drive = drive.len() == 2 && drive.ends_with(':');
            if is_drive && parts.len() >= 3 && parts[1].eq_ignore_ascii_case("Users") {
                3
            } else {
                return None;
            }
        }
    };
    Some(Foreign {
        style: path.style,
        parts: parts[..keep].to_vec(),
    })
}

/// Which rules count as the same: one `from` written two ways (`D:\Work` and `d:/work/`) is
/// one key.
pub fn rule_key(from: &str) -> String {
    match Foreign::parse(from) {
        Some(f) => {
            let (tag, parts): (&str, Vec<String>) = match f.style {
                Style::Unix => ("unix", f.parts),
                Style::Windows => ("windows", f.parts.iter().map(|p| p.to_ascii_lowercase()).collect()),
            };
            format!("{tag}:/{}", parts.join("/"))
        }
        None => format!("raw:{from}"),
    }
}

/// A rule from the trusted source (`PathMap` in cloud.proto).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRule {
    pub from: String,
    /// Empty: drop this device's own rule with this `from`.
    #[serde(default)]
    pub to: String,
    /// Device IDs; empty = every device.
    #[serde(default)]
    pub devices: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// `[[paths.map]]` in this device's config.toml.
    Local,
    /// The trusted source.
    Source,
}

/// One `from` as this device applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// As the side that applies wrote it.
    pub from: String,
    /// Empty: no rule for this `from` (the source dropped the device's own).
    pub to: String,
    pub origin: Origin,
    /// The device's own rule is locked.
    pub locked: bool,
    /// What the other side says, when both have a rule for this `from`.
    pub other: Option<String>,
}

/// What each side said per `from` the last time the device looked, and which applied.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub seen: BTreeMap<String, Seen>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub local: Option<String>,
    pub source: Option<String>,
    pub applied: Origin,
}

/// Which side applies when both have a value for one key: the device's own when it is locked;
/// otherwise the side that changed since `before`, the device's own when both did or this is
/// the first time both are seen ("the device's own first").
pub fn decide(local: &Option<String>, locked: bool, source: &Option<String>, before: Option<&Seen>) -> Origin {
    match (local, source) {
        (Some(_), _) if locked => Origin::Local,
        (Some(_), None) => Origin::Local,
        (None, _) => Origin::Source,
        (Some(_), Some(_)) => {
            let local_changed = before.is_none_or(|b| &b.local != local);
            let source_changed = before.is_none_or(|b| &b.source != source);
            match (local_changed, source_changed) {
                (false, true) => Origin::Source,
                (false, false) => before.map_or(Origin::Local, |b| b.applied),
                (true, _) => Origin::Local,
            }
        }
    }
}

/// The source's rules meant for `device`, one per key: a rule naming the device wins over one
/// for every device; otherwise the later one.
fn source_rules_for<'a>(source: &'a [SourceRule], device: &str) -> BTreeMap<String, &'a SourceRule> {
    let mut picked: BTreeMap<String, &SourceRule> = BTreeMap::new();
    for rule in source.iter().filter(|r| r.devices.is_empty()) {
        picked.insert(rule_key(&rule.from), rule);
    }
    for rule in source.iter().filter(|r| r.devices.iter().any(|d| d == device)) {
        picked.insert(rule_key(&rule.from), rule);
    }
    picked
}

/// The rules this device applies, and what to remember for next time.
pub fn merge(local: &[PathMap], source: &[SourceRule], device: &str, memory: &Memory) -> (Vec<Applied>, Memory) {
    let mut mine: BTreeMap<String, &PathMap> = BTreeMap::new();
    for rule in local {
        mine.insert(rule_key(&rule.from), rule); // the later of two wins, as in config.toml order
    }
    let theirs = source_rules_for(source, device);
    let mut keys: Vec<&String> = mine.keys().chain(theirs.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut applied = Vec::new();
    let mut next = Memory::default();
    for key in keys {
        let l = mine.get(key);
        let s = theirs.get(key);
        let (l_to, s_to) = (l.map(|r| r.to.clone()), s.map(|r| r.to.clone()));
        let winner = decide(&l_to, l.is_some_and(|l| l.lock), &s_to, memory.seen.get(key));
        let (from, to, other) = match winner {
            Origin::Local => {
                let l = l.expect("local wins only when it has a rule");
                (l.from.clone(), l.to.clone(), s_to.clone())
            }
            Origin::Source => {
                let s = s.expect("the source wins only when it has a rule");
                (s.from.clone(), s.to.clone(), l_to.clone())
            }
        };
        applied.push(Applied {
            from,
            to,
            origin: winner,
            locked: l.is_some_and(|l| l.lock),
            other,
        });
        next.seen.insert(
            key.clone(),
            Seen {
                local: l_to,
                source: s_to,
                applied: winner,
            },
        );
    }
    (applied, next)
}

/// What `Mapper` takes: the applied rules that map somewhere.
pub fn rules(applied: &[Applied]) -> Vec<PathMap> {
    applied
        .iter()
        .filter(|a| !a.to.is_empty())
        .map(|a| PathMap {
            from: a.from.clone(),
            to: a.to.clone(),
            lock: a.locked,
        })
        .collect()
}

/// Maps working directories of other devices to this one.
pub struct Mapper {
    rules: Vec<(Foreign, PathBuf)>,
    homes: Vec<Foreign>,
    local_home: PathBuf,
}

impl Mapper {
    /// `homes` are the home folders the storage's device files name; `local_home` is this
    /// device's.
    pub fn new(rules: &[PathMap], homes: &[String], local_home: &Path) -> Mapper {
        Mapper {
            rules: rules
                .iter()
                .filter_map(|r| Some((Foreign::parse(&r.from)?, PathBuf::from(&r.to))))
                .collect(),
            homes: homes.iter().filter_map(|h| Foreign::parse(h)).collect(),
            local_home: local_home.to_path_buf(),
        }
    }

    /// This device's path for `cwd`, or `None` when it is under no rule and no known home.
    pub fn map(&self, cwd: &str) -> Option<PathBuf> {
        let cwd = Foreign::parse(cwd)?;
        let longest = |candidates: Vec<(usize, &[String], PathBuf)>| {
            candidates
                .into_iter()
                .max_by_key(|(len, _, _)| *len)
                .map(|(_, rest, base)| rest.iter().fold(base, |path, part| path.join(part)))
        };
        let by_rule = self
            .rules
            .iter()
            .filter_map(|(from, to)| Some((from.parts.len(), cwd.under(from)?, to.clone())))
            .collect();
        if let Some(path) = longest(by_rule) {
            return Some(path);
        }
        let guessed = usual_home(&cwd);
        let by_home = self
            .homes
            .iter()
            .chain(guessed.as_ref())
            .filter_map(|home| Some((home.parts.len(), cwd.under(home)?, self.local_home.clone())))
            .collect();
        longest(by_home)
    }

    /// The Claude Code project folder for a session whose working directory was `cwd`, or
    /// `None` to keep the folder name it had.
    pub fn claude_folder(&self, cwd: &str) -> Option<String> {
        self.map(cwd).map(|path| claude_project_dir(&path.to_string_lossy()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_follow_claude_code() {
        assert_eq!(claude_project_dir("/Users/me/work/a.b"), "-Users-me-work-a-b");
        assert_eq!(
            claude_project_dir(r"C:\Users\me\AppData\Local\Temp\x"),
            "C--Users-me-AppData-Local-Temp-x"
        );
        // Every UTF-16 unit counts once: an emoji is two.
        assert_eq!(claude_project_dir("/a/你😀"), "-a----");
        // Long paths: cut at 200, then the hash of the whole path (JS `(h<<5)-h+c|0`, abs, base 36).
        let long = format!("/{}", "x".repeat(250));
        let name = claude_project_dir(&long);
        assert!(name.starts_with(&format!("-{}-", "x".repeat(199))));
        let mut h: i64 = 0;
        for c in long.chars() {
            h = (((h << 5) - h + c as i64) as i32) as i64;
        }
        assert_eq!(name[201..], base36(h.unsigned_abs()));
    }

    #[test]
    fn base36_matches_javascript() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(2_147_483_648), "zik0zk");
    }

    fn mapper(rules: &[(&str, &str)], homes: &[&str], local: &str) -> Mapper {
        let rules: Vec<PathMap> = rules
            .iter()
            .map(|(from, to)| PathMap {
                from: from.to_string(),
                to: to.to_string(),
                lock: false,
            })
            .collect();
        let homes: Vec<String> = homes.iter().map(|h| h.to_string()).collect();
        Mapper::new(&rules, &homes, Path::new(local))
    }

    #[test]
    fn a_path_under_the_source_home_moves_under_this_home() {
        let m = mapper(&[], &[], "/Users/me");
        assert_eq!(
            m.map(r"C:\Users\neko\work\a"),
            Some(Path::new("/Users/me").join("work").join("a"))
        );
        assert_eq!(
            m.map("/home/neko/src/x"),
            Some(Path::new("/Users/me").join("src").join("x"))
        );
        assert_eq!(m.map("/Users/me/p"), Some(Path::new("/Users/me").join("p")));
        assert_eq!(m.map("/root"), Some(PathBuf::from("/Users/me")));
        // Not under a home: keep the folder it had.
        assert_eq!(m.map("/tmp/x"), None);
        assert_eq!(m.map(r"D:\work\x"), None);
        assert_eq!(m.map("relative/path"), None);
        assert_eq!(
            m.claude_folder(r"C:\Users\neko\work\a").as_deref(),
            Some(claude_project_dir(&Path::new("/Users/me").join("work").join("a").to_string_lossy()).as_str())
        );
    }

    #[test]
    fn device_homes_and_rules_win_over_the_usual_layout() {
        let m = mapper(
            &[(r"D:\work", "/Users/me/w"), (r"d:\work\deep", "/Users/me/deep")],
            &["/data/users/neko", r"C:\Users\neko\AppData"],
            "/Users/me",
        );
        // A device file says this home; no usual layout would find it.
        assert_eq!(m.map("/data/users/neko/p"), Some(Path::new("/Users/me").join("p")));
        // The longest match wins, and Windows paths ignore case.
        assert_eq!(
            m.map(r"c:\users\neko\appdata\x"),
            Some(Path::new("/Users/me").join("x"))
        );
        assert_eq!(m.map(r"C:\Users\neko\y"), Some(Path::new("/Users/me").join("y")));
        assert_eq!(m.map(r"D:\Work\a"), Some(Path::new("/Users/me/w").join("a")));
        assert_eq!(m.map(r"D:\work\deep\b"), Some(Path::new("/Users/me/deep").join("b")));
        // Case matters on Unix.
        assert_eq!(m.map("/Data/users/neko/p"), None);
    }

    fn local(from: &str, to: &str, lock: bool) -> PathMap {
        PathMap {
            from: from.into(),
            to: to.into(),
            lock,
        }
    }

    fn source(from: &str, to: &str, devices: &[&str]) -> SourceRule {
        SourceRule {
            from: from.into(),
            to: to.into(),
            devices: devices.iter().map(|d| d.to_string()).collect(),
        }
    }

    /// `from` -> (`to`, origin) of what applies.
    fn table(applied: &[Applied]) -> Vec<(String, String, Origin)> {
        applied
            .iter()
            .map(|a| (a.from.clone(), a.to.clone(), a.origin))
            .collect()
    }

    #[test]
    fn rule_keys_ignore_how_a_path_is_written() {
        assert_eq!(rule_key(r"D:\Work\"), rule_key("d:/work"));
        assert_eq!(rule_key("/home/me/./src/"), rule_key("/home/me/src"));
        assert_ne!(rule_key("/Home/me"), rule_key("/home/me"), "case matters on Unix");
        assert_ne!(rule_key("/c/x"), rule_key(r"C:\x"));
        assert_eq!(rule_key("relative"), "raw:relative");
    }

    #[test]
    fn each_side_alone_applies() {
        let (applied, _) = merge(
            &[local("/a", "/x", false)],
            &[source("/b", "/y", &[]), source("/c", "/z", &["other"])],
            "me",
            &Memory::default(),
        );
        assert_eq!(
            table(&applied),
            vec![
                ("/a".into(), "/x".into(), Origin::Local),
                ("/b".into(), "/y".into(), Origin::Source),
            ],
            "a rule for another device does not apply here"
        );
        assert_eq!(rules(&applied).len(), 2);
    }

    #[test]
    fn the_device_first_then_whoever_changed_last() {
        let mine = [local(r"D:\work", "/Users/me/w", false)];
        // Seen together for the first time: the device's own.
        let (applied, memory) = merge(&mine, &[source("d:/work", "/srv/w", &["me"])], "me", &Memory::default());
        assert_eq!(
            table(&applied),
            vec![(r"D:\work".into(), "/Users/me/w".into(), Origin::Local)]
        );
        assert_eq!(applied[0].other.as_deref(), Some("/srv/w"));

        // The source changes its rule: the source's applies.
        let (applied, memory) = merge(&mine, &[source("d:/work", "/srv/w2", &["me"])], "me", &memory);
        assert_eq!(
            table(&applied),
            vec![("d:/work".into(), "/srv/w2".into(), Origin::Source)]
        );
        // Nothing changes: it stays.
        let (applied, memory) = merge(&mine, &[source("d:/work", "/srv/w2", &["me"])], "me", &memory);
        assert_eq!(applied[0].origin, Origin::Source);

        // The device's own changes: it applies again.
        let mine2 = [local(r"D:\work", "/Users/me/w3", false)];
        let (applied, memory) = merge(&mine2, &[source("d:/work", "/srv/w2", &["me"])], "me", &memory);
        assert_eq!(
            table(&applied),
            vec![(r"D:\work".into(), "/Users/me/w3".into(), Origin::Local)]
        );

        // The source drops its rule: the device's own, then the source's again once it is back.
        let (applied, memory) = merge(&mine2, &[], "me", &memory);
        assert_eq!(applied[0].origin, Origin::Local);
        let (applied, _) = merge(&mine2, &[source("d:/work", "/srv/w2", &["me"])], "me", &memory);
        assert_eq!(applied[0].origin, Origin::Source);
    }

    #[test]
    fn a_source_rule_that_arrives_after_the_device_had_its_own_applies() {
        let mine = [local("/home/me/src", "/Users/me/src", false)];
        let (_, memory) = merge(&mine, &[], "me", &Memory::default());
        let (applied, _) = merge(&mine, &[source("/home/me/src", "/Volumes/src", &[])], "me", &memory);
        assert_eq!(applied[0].origin, Origin::Source);
        assert_eq!(applied[0].other.as_deref(), Some("/Users/me/src"));
    }

    #[test]
    fn a_locked_rule_always_applies() {
        let mine = [local("/a", "/x", true)];
        let (_, memory) = merge(&mine, &[], "me", &Memory::default());
        let (applied, _) = merge(&mine, &[source("/a", "/y", &["me"])], "me", &memory);
        assert_eq!(table(&applied), vec![("/a".into(), "/x".into(), Origin::Local)]);
        assert!(applied[0].locked);
    }

    #[test]
    fn an_empty_to_drops_the_devices_own_rule() {
        let mine = [local("/a", "/x", false), local("/b", "/y", false)];
        let (_, memory) = merge(&mine, &[], "me", &Memory::default());
        let (applied, _) = merge(&mine, &[source("/a", "", &["me"])], "me", &memory);
        assert_eq!(applied[0].to, "");
        assert_eq!(applied[0].origin, Origin::Source);
        let kept = rules(&applied);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].from, "/b");
        // With nothing of its own, a drop is simply no rule.
        let (applied, _) = merge(&[], &[source("/a", "", &[])], "me", &Memory::default());
        assert!(rules(&applied).is_empty());
    }

    #[test]
    fn a_rule_for_this_device_wins_over_one_for_every_device() {
        let src = [source("/a", "/for-me", &["me"]), source("/a", "/for-all", &[])];
        let (applied, _) = merge(&[], &src, "me", &Memory::default());
        assert_eq!(applied[0].to, "/for-me");
        let (applied, _) = merge(&[], &src, "other", &Memory::default());
        assert_eq!(applied[0].to, "/for-all");
    }

    #[test]
    fn merged_rules_feed_the_mapper() {
        let (applied, _) = merge(
            &[local(r"D:\work", "/Users/me/w", false)],
            &[source("/data/proj", "/Users/me/proj", &[])],
            "me",
            &Memory::default(),
        );
        let m = Mapper::new(&rules(&applied), &[], Path::new("/Users/me"));
        assert_eq!(m.map(r"D:\work\a"), Some(Path::new("/Users/me/w").join("a")));
        assert_eq!(m.map("/data/proj/x"), Some(Path::new("/Users/me/proj").join("x")));
    }
}
