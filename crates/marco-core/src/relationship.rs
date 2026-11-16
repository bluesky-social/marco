//! Relationships come from config too: what `Relationships` reports between
//! an actor and each other when a request names none, and what counts as a
//! block in `BlockPairs`.
//!
//! ```toml
//! [[relationship]]
//! name = "follow"
//! kind = "direct"
//! relation = "follow"
//!
//! [[relationship]]
//! name = "block"
//! kind = "direct"
//! relation = "block"
//! blocks = true
//!
//! [[relationship]]
//! name = "block_by_list"
//! kind = "list"
//! subscription = "listblock"
//! membership = "listitem"
//! list_filter = "modlist"
//! blocks = true
//! ```
//!
//! A `direct` relationship is one relation both ways: the actor's record
//! naming the other, and the other's naming the actor. A `list` one is a
//! via: a list the actor subscribes to that holds the other, and one the
//! other subscribes to that holds the actor, answered with the list's URI.
//! Relationships change no stored edge, so a new config needs a restart,
//! not a rebuild.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::graph::{PairQuery, ViaLists, ViaQuery};
use crate::relation::{RelId, Relations};
use crate::Dir;

/// The config marco serves with when none is given: the relations between
/// actors, and blocks by mod list.
pub const DEFAULT_CONFIG: &str = r#"[[relationship]]
name = "follow"
kind = "direct"
relation = "follow"

[[relationship]]
name = "block"
kind = "direct"
relation = "block"
blocks = true

[[relationship]]
name = "block_by_list"
kind = "list"
subscription = "listblock"
membership = "listitem"
list_filter = "modlist"
blocks = true
"#;

/// The kinds, and the keys each takes beside [`COMMON_KEYS`].
pub const KINDS: [(&str, &[&str]); 2] =
    [("direct", &["relation"]), ("list", &["subscription", "membership", "list_filter"])];

/// The keys every `[[relationship]]` takes.
pub const COMMON_KEYS: [&str; 5] = ["name", "kind", "blocks", "default", "private"];

/// One `[[relationship]]` table, as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipDef {
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub membership: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_filter: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub blocks: bool,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub default: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub private: bool,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Config {
    #[serde(rename = "relationship", default)]
    list: Vec<RelationshipDef>,
}

/// How a relationship is found, its relations resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Derive {
    /// The actor's record of the relation naming the other, and back.
    Direct(RelId),
    /// A via: lists in `subscription` (a relation whose subjects are
    /// lists) whose `membership` edges hold the other side; with `filter`,
    /// only a list a live record of that relation names.
    List { subscription: RelId, membership: RelId, filter: Option<RelId> },
}

impl Derive {
    /// A list relationship's via in one direction.
    pub fn via(&self, dir: Dir) -> Option<ViaQuery> {
        match *self {
            Derive::Direct(_) => None,
            Derive::List { subscription, membership, filter } => {
                Some(ViaQuery { lists: ViaLists::Rel(subscription), members: membership, kind: filter, dir })
            }
        }
    }

    /// A list relationship's check of both sides of a pair.
    pub fn pair(&self) -> Option<PairQuery> {
        match *self {
            Derive::Direct(_) => None,
            Derive::List { subscription, membership, filter } => {
                Some(PairQuery { rel: subscription, members: membership, kind: filter })
            }
        }
    }
}

/// A relationship ready to answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relationship {
    pub name: String,
    pub derive: Derive,
    /// Counts in `BlockPairs` when its request names no relationships.
    pub blocks: bool,
    /// In `Relationships` when its request names none.
    pub default: bool,
    /// Only the actor's side: what the other has toward the actor (its
    /// mutes, say) isn't the actor's to see.
    pub private: bool,
}

/// The relationships a marco serves.
#[derive(Clone, Debug, Default)]
pub struct RelationshipSet {
    pub list: Vec<Relationship>,
    defs: Vec<RelationshipDef>,
}

/// A config file's `[[relationship]]` tables. A key a kind doesn't take is
/// an error, as a relation's is, so a typo can't quietly drop a filter.
pub fn parse_toml(s: &str) -> Result<Vec<RelationshipDef>> {
    let t: toml::Table = toml::from_str(s).context("relationships config")?;
    if let Some(k) = t.keys().find(|k| *k != "relationship") {
        bail!("relationships config: unknown key {k:?}: relationships are [[relationship]] tables");
    }
    for (i, r) in t.get("relationship").and_then(|r| r.as_array()).into_iter().flatten().enumerate() {
        let name = r.get("name").and_then(|n| n.as_str()).map_or_else(|| i.to_string(), str::to_string);
        let Some(kind) = r.get("kind").and_then(|k| k.as_str()) else {
            bail!("relationship {name}: kind is required (one of {})", kind_names());
        };
        let Some((_, own)) = KINDS.iter().find(|(k, _)| *k == kind) else {
            bail!("relationship {name}: unknown kind {kind:?} (the kinds are {})", kind_names());
        };
        let keys: Vec<&str> = COMMON_KEYS.iter().chain(own.iter()).copied().collect();
        if let Some(k) = r.as_table().and_then(|r| r.keys().find(|k| !keys.contains(&k.as_str()))) {
            bail!("relationship {name}: a {kind} relationship takes no {k:?} (its keys are {})", keys.join(", "));
        }
    }
    let c: Config = t.try_into().context("relationships config")?;
    Ok(c.list)
}

fn kind_names() -> String {
    KINDS.iter().map(|k| k.0).collect::<Vec<_>>().join(", ")
}

impl RelationshipSet {
    /// `defs` over a snapshot's relations. Every relation they name has to
    /// be there, and be one the kind can read.
    pub fn resolve(defs: Vec<RelationshipDef>, rels: &Relations) -> Result<RelationshipSet> {
        let mut list = Vec::with_capacity(defs.len());
        for (i, d) in defs.iter().enumerate() {
            if d.name.is_empty() {
                bail!("relationship {i}: name is required");
            }
            if defs[..i].iter().any(|o| o.name == d.name) {
                bail!("relationship {}: names must be unique", d.name);
            }
            let derive = derive(d, rels).with_context(|| format!("relationship {}", d.name))?;
            if let Some(r) = rels.by_name(&d.name) {
                if derive != Derive::Direct(r) {
                    bail!(
                        "relationship {}: a relation has that name, so only a direct relationship over it can",
                        d.name
                    );
                }
            }
            if d.private && d.blocks {
                bail!("relationship {}: a block is both sides' to see, so a private relationship can't block", d.name);
            }
            list.push(Relationship {
                name: d.name.clone(),
                derive,
                blocks: d.blocks,
                default: d.default,
                private: d.private,
            });
        }
        Ok(RelationshipSet { list, defs })
    }

    /// [`DEFAULT_CONFIG`], less what the snapshot has no relations for: the
    /// set, and the names it left out.
    pub fn bsky_default(rels: &Relations) -> (RelationshipSet, Vec<String>) {
        let defs = parse_toml(DEFAULT_CONFIG).expect("default relationships");
        let (keep, drop): (Vec<_>, Vec<_>) = defs.into_iter().partition(|d| derive(d, rels).is_ok());
        let set = RelationshipSet::resolve(keep, rels).expect("default relationships resolve");
        (set, drop.into_iter().map(|d| d.name).collect())
    }

    pub fn by_name(&self, name: &str) -> Option<&Relationship> {
        self.list.iter().find(|r| r.name == name)
    }

    /// What `Relationships` reports when a request names nothing.
    pub fn defaults(&self) -> Vec<Relationship> {
        self.list.iter().filter(|r| r.default).cloned().collect()
    }

    /// What counts as a block when a `BlockPairs` request names nothing.
    pub fn blocking(&self) -> Vec<Relationship> {
        self.list.iter().filter(|r| r.blocks).cloned().collect()
    }

    /// The relationships `names` names. A name that isn't one but is a
    /// relation's is that relation as a direct relationship, so a request
    /// can name any relation.
    pub fn named(&self, rels: &Relations, names: &[String]) -> Result<Vec<Relationship>, String> {
        names
            .iter()
            .map(|n| {
                if n.is_empty() {
                    return Err("rel is required".to_string());
                }
                if let Some(r) = self.by_name(n) {
                    return Ok(r.clone());
                }
                match rels.by_name(n) {
                    Some(r) => Ok(Relationship {
                        name: n.clone(),
                        derive: Derive::Direct(r),
                        blocks: false,
                        default: false,
                        private: false,
                    }),
                    None => Err(format!("no relationship or relation {n:?}")),
                }
            })
            .collect()
    }

    /// The config, as TOML.
    pub fn to_toml(&self) -> String {
        toml::to_string(&Config { list: self.defs.clone() }).unwrap_or_default()
    }

    pub fn defs(&self) -> &[RelationshipDef] {
        &self.defs
    }
}

fn derive(d: &RelationshipDef, rels: &Relations) -> Result<Derive> {
    let get = |key: &str, v: &Option<String>| -> Result<RelId> {
        let name = v.as_deref().with_context(|| format!("a {} relationship needs {key}", d.kind))?;
        rels.by_name(name).with_context(|| format!("{key} {name:?}: the snapshot has no such relation"))
    };
    let named = |r: RelId| &rels.get(r).name;
    match d.kind.as_str() {
        "direct" => {
            let r = get("relation", &d.relation)?;
            if !rels.actor_rels().contains(&r) {
                bail!("relation {:?} isn't between actors: no list relation, companion or record subjects", named(r));
            }
            Ok(Derive::Direct(r))
        }
        "list" => {
            let subscription = get("subscription", &d.subscription)?;
            let s = rels.get(subscription);
            if s.subject_collection.is_none() {
                bail!("subscription {:?} has to take lists as subjects (subject_collection)", s.name);
            }
            let membership = get("membership", &d.membership)?;
            if !rels.is_list(membership) {
                bail!("membership {:?} has to be a list relation (list = …)", named(membership));
            }
            let filter = match &d.list_filter {
                None => None,
                Some(_) => {
                    let f = get("list_filter", &d.list_filter)?;
                    if !rels.is_uri_subject(f) {
                        bail!("list_filter {:?} has to name records (subject_collection or $self)", named(f));
                    }
                    Some(f)
                }
            };
            Ok(Derive::List { subscription, membership, filter })
        }
        k => bail!("unknown kind {k:?} (the kinds are {})", kind_names()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTS: &str = r#"
[[relation]]
name = "follow"
collection = "app.bsky.graph.follow"
subject = "subject"

[[relation]]
name = "block"
collection = "app.bsky.graph.block"
subject = "subject"

[[relation]]
name = "listitem"
collection = "app.bsky.graph.listitem"
subject = "subject"
list = "list"

[[relation]]
name = "listblock"
collection = "app.bsky.graph.listblock"
subject = "subject"
subject_collection = "app.bsky.graph.list"

[[relation]]
name = "modlist"
collection = "app.bsky.graph.list"
subject = "$self"
when = { purpose = "app.bsky.graph.defs#modlist" }
"#;

    #[test]
    fn the_default() {
        let rels = Relations::parse_toml(LISTS).unwrap();
        let (set, dropped) = RelationshipSet::bsky_default(&rels);
        assert!(dropped.is_empty());
        let names: Vec<&str> = set.list.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["follow", "block", "block_by_list"]);
        assert_eq!(set.list[0].derive, Derive::Direct(0));
        let lb = set.list[2].derive;
        assert_eq!(lb, Derive::List { subscription: 4, membership: 2, filter: Some(5) });
        assert_eq!(lb.pair(), Some(PairQuery { rel: 4, members: 2, kind: Some(5) }));
        assert_eq!(lb.via(Dir::In).map(|q| q.dir), Some(Dir::In));
        assert_eq!(Derive::Direct(0).via(Dir::Out), None);
        let blocks: Vec<String> = set.blocking().into_iter().map(|r| r.name).collect();
        assert_eq!(blocks, ["block", "block_by_list"]);
        assert_eq!(set.defaults().len(), 3);
        assert_eq!(parse_toml(&set.to_toml()).unwrap(), parse_toml(DEFAULT_CONFIG).unwrap());

        // a snapshot of the default relations has no subscriptions to read
        let (set, dropped) = RelationshipSet::bsky_default(&Relations::bsky_default());
        assert_eq!(dropped, ["block_by_list"]);
        assert_eq!(set.list.len(), 2);
    }

    #[test]
    fn names_fall_back_to_relations() {
        let rels = Relations::parse_toml(LISTS).unwrap();
        let (set, _) = RelationshipSet::bsky_default(&rels);
        let n = |v: &[&str]| set.named(&rels, &v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let got = n(&["block_by_list", "listitem", "follow"]).unwrap();
        assert_eq!(got[1].derive, Derive::Direct(2), "a relation, as a direct relationship");
        assert!(!got[1].blocks && got[0].blocks);
        assert!(n(&["nope"]).unwrap_err().contains("nope"));
        assert!(n(&[""]).is_err());
    }

    #[test]
    fn refuses_bad_config() {
        let rels = Relations::parse_toml(LISTS).unwrap();
        let one = |s: &str| parse_toml(s).and_then(|d| RelationshipSet::resolve(d, &rels));
        let ok = "[[relationship]]\nname = 'b'\nkind = 'list'\nsubscription = 'listblock'\nmembership = 'listitem'\n";
        assert!(one(ok).is_ok());
        let e = |s: &str| format!("{:#}", one(s).unwrap_err());
        assert!(e("[[relationship]]\nname = 'x'\nrelation = 'follow'").contains("kind is required"));
        assert!(e("[[relationship]]\nname = 'x'\nkind = 'chain'").contains("unknown kind"));
        let both = "[[relationship]]\nname = 'x'\nkind = 'direct'\nrelation = 'follow'\nmembership = 'listitem'";
        assert!(e(both).contains("takes no \"membership\""));
        assert!(e("[[relationship]]\nname = 'x'\nkind = 'direct'\nrelation = 'mute'").contains("no such relation"));
        assert!(e("[[relationship]]\nname = 'x'\nkind = 'direct'\nrelation = 'listitem'").contains("between actors"));
        assert!(e("[[relationship]]\nname = 'x'\nkind = 'direct'").contains("needs relation"));
        let bad_sub = "[[relationship]]\nname = 'x'\nkind = 'list'\nsubscription = 'block'\nmembership = 'listitem'";
        assert!(e(bad_sub).contains("subscription"));
        let bad_mem = "[[relationship]]\nname = 'x'\nkind = 'list'\nsubscription = 'listblock'\nmembership = 'block'";
        assert!(e(bad_mem).contains("membership"));
        assert!(e(&format!("{ok}list_filter = 'follow'\n")).contains("list_filter"));
        assert!(e(&format!("{ok}{ok}")).contains("unique"));
        let shadow =
            "[[relationship]]\nname = 'listblock'\nkind = 'list'\nsubscription = 'listblock'\nmembership = 'listitem'";
        assert!(e(shadow).contains("a relation has that name"));
        assert!(one("[[relationship]]\nname = 'follow'\nkind = 'direct'\nrelation = 'follow'").is_ok());
        assert!(e(&format!("{ok}private = true\nblocks = true\n")).contains("can't block"));
        assert!(e("[[relationships]]\nname = 'x'").contains("unknown key"));
        let typed = "[[relationship]]\nname = 'x'\nkind = 'direct'\nrelation = 'follow'\nblocks = 'yes'";
        assert!(e(typed).contains("config"));
    }
}
