//! Values the trusted source pushes that this device can also set itself:
//! `local.copy_form`, `update.channel`, `update.urls`. The device's own value is whatever
//! config.toml says for that key; `nekorisu config set` writes it, and `--lock` keeps the
//! source's value from applying. They merge like path maps (`pathmap::decide`), except
//! `update.urls`, where the source's addresses are tried before the device's own (cloud.proto).

use std::collections::BTreeMap;

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};

use crate::config::{Config, PathMap};
use crate::pathmap::{self, Memory, Origin, Seen};

pub const KEYS: &[&str] = &["local.copy_form", "update.channel", "update.urls"];
/// The key a forced path rule names; its value is `{"from": ..., "to": ...}`.
pub const PATHS_KEY: &str = "paths.map";

/// Checks a value for `key`, in the JSON form cloud.proto uses (`value_json`).
pub fn check(key: &str, value: &Value) -> anyhow::Result<()> {
    match key {
        "local.copy_form" => match value.as_str() {
            Some("pruned" | "full") => Ok(()),
            _ => bail!("local.copy_form 只能是 pruned 或 full"),
        },
        "update.channel" => match value.as_str() {
            Some("stable" | "beta") => Ok(()),
            _ => bail!("update.channel 只能是 stable 或 beta"),
        },
        "update.urls" => {
            let urls = value.as_array().context("update.urls 是一组地址")?;
            for url in urls {
                let text = url.as_str().context("update.urls 里每一项都是地址")?;
                let parsed = reqwest::Url::parse(text).with_context(|| format!("{text} 不是地址"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("{text} 要以 http:// 或 https:// 开头");
                }
            }
            Ok(())
        }
        other => bail!("不认识的键 {other}；能设置的是 {}", KEYS.join("、")),
    }
}

/// What `nekorisu config set <key> <value>` means: a list for `update.urls` (comma-separated
/// or a JSON array), text otherwise.
pub fn parse_cli(key: &str, text: &str) -> anyhow::Result<Value> {
    let value = if key == "update.urls" {
        match serde_json::from_str::<Value>(text) {
            Ok(v @ Value::Array(_)) => v,
            _ => Value::Array(
                text.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.to_string()))
                    .collect(),
            ),
        }
    } else {
        Value::String(text.trim().to_string())
    };
    check(key, &value)?;
    Ok(value)
}

fn split(key: &str) -> (&str, &str) {
    key.split_once('.').expect("keys are section.name")
}

fn to_toml(value: &Value) -> anyhow::Result<toml::Value> {
    Ok(match value {
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(items) => toml::Value::Array(items.iter().map(to_toml).collect::<anyhow::Result<_>>()?),
        Value::Bool(b) => toml::Value::Boolean(*b),
        other => bail!("这个值放不进 config.toml：{other}"),
    })
}

fn to_json(value: &toml::Value) -> Option<Value> {
    Some(match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Array(items) => Value::Array(items.iter().map(to_json).collect::<Option<_>>()?),
        toml::Value::Boolean(b) => Value::Bool(*b),
        _ => return None,
    })
}

/// The device's own values: the keys config.toml sets.
pub fn local_values(table: &toml::Table) -> BTreeMap<String, Value> {
    KEYS.iter()
        .filter_map(|key| {
            let (section, name) = split(key);
            let value = table.get(section)?.as_table()?.get(name)?;
            let json = to_json(value)?;
            check(key, &json).ok()?;
            Some((key.to_string(), json))
        })
        .collect()
}

pub fn locked(table: &toml::Table) -> Vec<String> {
    table
        .get("locked")
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// Sets (`Some`) or removes (`None`) the device's own value for `key` in config.toml's table,
/// and locks or unlocks it when `lock` says so.
pub fn set(table: &mut toml::Table, key: &str, value: Option<&Value>, lock: Option<bool>) -> anyhow::Result<()> {
    if !KEYS.contains(&key) {
        bail!("不认识的键 {key}；能设置的是 {}", KEYS.join("、"));
    }
    let (section, name) = split(key);
    match value {
        Some(value) => {
            check(key, value)?;
            table
                .entry(section)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
                .with_context(|| format!("config.toml 里的 {section} 应该是一张表"))?
                .insert(name.to_string(), to_toml(value)?);
        }
        None => {
            if let Some(t) = table.get_mut(section).and_then(toml::Value::as_table_mut) {
                t.remove(name);
            }
        }
    }
    if let Some(lock) = lock {
        let mut keys = locked(table);
        keys.retain(|k| k != key);
        if lock {
            keys.push(key.to_string());
        }
        if keys.is_empty() {
            table.remove("locked");
        } else {
            table.insert(
                "locked".into(),
                toml::Value::Array(keys.into_iter().map(toml::Value::String).collect()),
            );
        }
    }
    Ok(())
}

/// Applies one forced change to config.toml's table: the value becomes the device's own and
/// the key is unlocked. For `paths.map` the value is one rule; an empty `to` removes the rule.
pub fn force(table: &mut toml::Table, key: &str, value_json: &str) -> anyhow::Result<()> {
    let value: Value = serde_json::from_str(value_json).with_context(|| format!("{key} 的值不是 JSON"))?;
    if key != PATHS_KEY {
        return set(table, key, Some(&value), Some(false));
    }
    let from = value["from"]
        .as_str()
        .filter(|f| !f.trim().is_empty())
        .context("强制的路径映射没有 from")?;
    let to = value["to"].as_str().unwrap_or_default();
    let wanted = pathmap::rule_key(from);
    let rules = table
        .entry("paths")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("config.toml 里的 paths 应该是一张表")?
        .entry("map")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .context("config.toml 里的 paths.map 应该是 [[paths.map]]")?;
    let mut found = false;
    rules.retain_mut(|rule| {
        let Some(rule) = rule.as_table_mut() else { return true };
        let same = rule.get("from").and_then(toml::Value::as_str).map(pathmap::rule_key) == Some(wanted.clone());
        if !same {
            return true;
        }
        found = true;
        if to.is_empty() {
            return false;
        }
        rule.insert("to".into(), toml::Value::String(to.to_string()));
        rule.remove("lock");
        true
    });
    if !found && !to.is_empty() {
        let mut rule = toml::Table::new();
        rule.insert("from".into(), toml::Value::String(from.to_string()));
        rule.insert("to".into(), toml::Value::String(to.to_string()));
        rules.push(toml::Value::Table(rule));
    }
    Ok(())
}

/// Whether this version knows how to apply a forced `key`.
pub fn can_force(key: &str) -> bool {
    key == PATHS_KEY || KEYS.contains(&key)
}

/// One key as this device uses it.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    pub key: String,
    pub value: Value,
    pub origin: Origin,
    pub locked: bool,
    /// What the other side says, when both have a value.
    pub other: Option<Value>,
}

/// The values this device uses, and what to remember for next time.
pub fn merge(
    local: &BTreeMap<String, Value>,
    locked: &[String],
    source: &BTreeMap<String, Value>,
    memory: &Memory,
) -> (Vec<Applied>, Memory) {
    let mut applied = Vec::new();
    let mut next = Memory::default();
    for key in KEYS {
        let (l, s) = (local.get(*key), source.get(*key));
        if l.is_none() && s.is_none() {
            continue;
        }
        let is_locked = locked.iter().any(|k| k == key);
        let text = |v: Option<&Value>| v.map(Value::to_string);
        let (l_text, s_text) = (text(l), text(s));
        let origin = pathmap::decide(&l_text, is_locked, &s_text, memory.seen.get(*key));
        let (value, other) = if *key == "update.urls" && !is_locked && l.is_some() && s.is_some() {
            // The source's addresses first, then the device's own.
            let mut urls: Vec<Value> = s.and_then(Value::as_array).cloned().unwrap_or_default();
            for url in l.and_then(Value::as_array).into_iter().flatten() {
                if !urls.contains(url) {
                    urls.push(url.clone());
                }
            }
            (Value::Array(urls), None)
        } else {
            match origin {
                Origin::Local => (l.cloned().unwrap_or(Value::Null), s.cloned()),
                Origin::Source => (s.cloned().unwrap_or(Value::Null), l.cloned()),
            }
        };
        applied.push(Applied {
            key: key.to_string(),
            value,
            origin: if *key == "update.urls" && !is_locked && s.is_some() {
                Origin::Source
            } else {
                origin
            },
            locked: is_locked,
            other,
        });
        next.seen.insert(
            key.to_string(),
            Seen {
                local: l_text,
                source: s_text,
                applied: origin,
            },
        );
    }
    (applied, next)
}

/// `config` with the merged values in place.
pub fn apply(config: &mut Config, applied: &[Applied]) {
    for a in applied {
        match (a.key.as_str(), &a.value) {
            ("local.copy_form", Value::String(v)) => config.local.copy_form = v.clone(),
            ("update.channel", Value::String(v)) => config.update.channel = v.clone(),
            ("update.urls", Value::Array(items)) => {
                config.update.urls = items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
            }
            _ => {}
        }
    }
}

/// What the report carries about this device's own values: key, JSON text, locked.
pub fn report_values(table: &toml::Table) -> Vec<(String, String, bool)> {
    let locked = locked(table);
    local_values(table)
        .into_iter()
        .map(|(key, value)| {
            let is_locked = locked.contains(&key);
            (key, value.to_string(), is_locked)
        })
        .collect()
}

/// The source's values for these keys, from a config it pushed.
pub fn source_values(copy_form: &str, channel: &str, urls: &[String]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    if !copy_form.is_empty() {
        out.insert("local.copy_form".to_string(), json!(copy_form));
    }
    if !channel.is_empty() {
        out.insert("update.channel".to_string(), json!(channel));
    }
    if !urls.is_empty() {
        out.insert("update.urls".to_string(), json!(urls));
    }
    out.retain(|key, value| check(key, value).is_ok());
    out
}

/// The path rules in config.toml's table (for tests and the report).
#[allow(dead_code)]
pub fn path_rules(table: &toml::Table) -> anyhow::Result<Vec<PathMap>> {
    let config: Config = toml::Value::Table(table.clone())
        .try_into()
        .map_err(|e| anyhow!("config.toml 读不了：{e}"))?;
    Ok(config.paths.map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    #[test]
    fn values_are_checked() {
        assert!(parse_cli("local.copy_form", "full").is_ok());
        assert!(parse_cli("local.copy_form", "half").is_err());
        assert!(parse_cli("update.channel", "beta").is_ok());
        assert_eq!(
            parse_cli("update.urls", "https://a.example/x.json, https://b.example/y.json").unwrap(),
            json!(["https://a.example/x.json", "https://b.example/y.json"])
        );
        assert!(parse_cli("update.urls", "ftp://x").is_err());
        assert!(
            parse_cli("source.url", "x").is_err(),
            "trust roots are never settable here"
        );
    }

    #[test]
    fn set_and_lock_write_config_toml() {
        let mut t = table("[storage.nas]\nkind = \"webdav\"\nurl = \"https://dav.example\"\n");
        set(&mut t, "local.copy_form", Some(&json!("full")), Some(true)).unwrap();
        set(&mut t, "update.urls", Some(&json!(["https://m.example/s.json"])), None).unwrap();
        assert_eq!(locked(&t), vec!["local.copy_form".to_string()]);
        let values = local_values(&t);
        assert_eq!(values["local.copy_form"], json!("full"));
        assert_eq!(values["update.urls"], json!(["https://m.example/s.json"]));
        assert!(
            !values.contains_key("update.channel"),
            "not written, not the device's own"
        );
        set(&mut t, "local.copy_form", None, Some(false)).unwrap();
        assert!(!local_values(&t).contains_key("local.copy_form"));
        assert!(t.get("locked").is_none());
        // Other sections stay.
        assert!(t["storage"]["nas"].is_table());
        let config: Config = toml::Value::Table(t).try_into().unwrap();
        assert_eq!(config.update.urls.len(), 1);
    }

    #[test]
    fn merging_follows_the_same_rule_as_path_maps() {
        let local = BTreeMap::from([("local.copy_form".to_string(), json!("full"))]);
        let none = BTreeMap::new();
        let src = |v: &str| BTreeMap::from([("local.copy_form".to_string(), json!(v))]);

        let (applied, memory) = merge(&local, &[], &none, &Memory::default());
        assert_eq!(
            (applied[0].value.clone(), applied[0].origin),
            (json!("full"), Origin::Local)
        );
        // The source's value arrives after the device's own: it applies.
        let (applied, memory) = merge(&local, &[], &src("pruned"), &memory);
        assert_eq!(
            (applied[0].value.clone(), applied[0].origin),
            (json!("pruned"), Origin::Source)
        );
        assert_eq!(applied[0].other, Some(json!("full")));
        // Locked: the device's own.
        let (applied, _) = merge(&local, &["local.copy_form".to_string()], &src("pruned"), &memory);
        assert_eq!(applied[0].origin, Origin::Local);
        // Only the source has one.
        let (applied, _) = merge(&none, &[], &src("pruned"), &Memory::default());
        assert_eq!(applied[0].value, json!("pruned"));
        let mut config = Config::default();
        apply(&mut config, &applied);
        assert_eq!(config.local.copy_form, "pruned");
    }

    #[test]
    fn update_addresses_from_the_source_come_first() {
        let local = BTreeMap::from([("update.urls".to_string(), json!(["https://mine.example/s.json"]))]);
        let source = source_values("", "", &["https://theirs.example/s.json".to_string()]);
        let (applied, _) = merge(&local, &[], &source, &Memory::default());
        assert_eq!(
            applied[0].value,
            json!(["https://theirs.example/s.json", "https://mine.example/s.json"])
        );
        let (applied, _) = merge(&local, &["update.urls".to_string()], &source, &Memory::default());
        assert_eq!(
            applied[0].value,
            json!(["https://mine.example/s.json"]),
            "locked: only its own"
        );
    }

    #[test]
    fn a_forced_value_becomes_the_devices_own_and_unlocks() {
        let mut t = table(
            "locked = [\"local.copy_form\"]\n[local]\ncopy_form = \"full\"\n\
             [[paths.map]]\nfrom = 'D:\\work'\nto = \"/mine\"\nlock = true\n\
             [[paths.map]]\nfrom = \"/keep\"\nto = \"/kept\"\n",
        );
        force(&mut t, "local.copy_form", "\"pruned\"").unwrap();
        assert_eq!(local_values(&t)["local.copy_form"], json!("pruned"));
        assert!(locked(&t).is_empty());

        // Same rule written another way: replaced and unlocked.
        force(&mut t, PATHS_KEY, r#"{"from":"d:/work/","to":"/theirs"}"#).unwrap();
        let rules = path_rules(&t).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!((rules[0].to.as_str(), rules[0].lock), ("/theirs", false));
        // An empty `to` removes it; a new `from` is added.
        force(&mut t, PATHS_KEY, r#"{"from":"/keep","to":""}"#).unwrap();
        force(&mut t, PATHS_KEY, r#"{"from":"/new","to":"/n"}"#).unwrap();
        let rules = path_rules(&t).unwrap();
        assert_eq!(
            rules.iter().map(|r| r.from.as_str()).collect::<Vec<_>>(),
            vec![r"D:\work", "/new"]
        );
        assert!(force(&mut t, PATHS_KEY, r#"{"to":"/x"}"#).is_err());
        assert!(!can_force("digest.excerpt"));
    }
}
