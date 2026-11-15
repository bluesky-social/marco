//! Via: a list a subscriber is connected to that holds a member, both
//! directions, from a relation or from the caller's lists, over a snapshot
//! and over deltas.

use marco_core::graph::{VIA_ACTOR_CAP, VIA_MASKED_PER_RECORD, VIA_OTHER_CAP};
use marco_core::{Dir, Graph, Relations, Status, ViaLists, ViaQuery, ViaTruncated};
use vlatproto::tid::Tid;

const LIST: u8 = 2;
const LISTBLOCK: u8 = 4;
const MODLIST: u8 = 5;

fn rels() -> Relations {
    Relations::parse_toml(
        r#"
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
"#,
    )
    .unwrap()
}

fn did(i: u32) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut v = (i as u64 + 7).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let s: String = (0..24)
        .map(|_| {
            let c = alphabet[(v & 31) as usize] as char;
            v = v.rotate_right(5) ^ 0x5bd1_e995;
            c
        })
        .collect();
    format!("did:plc:{s}")
}

fn tid(t: u64) -> String {
    Tid::from_parts(1_700_000_000_000_000 + t * 1000, 0).to_string()
}

fn list(owner: &str, l: u32) -> String {
    format!("at://{owner}/app.bsky.graph.list/l{l}")
}

fn graph() -> Graph {
    let mut g = Graph::new(rels());
    g.shards = 3;
    g
}

fn reopen(g: &Graph) -> Graph {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("g.marco");
    g.write_snapshot(&p).unwrap();
    let mut g2 = Graph::open(&p).unwrap();
    g2.shards = g.shards;
    g2
}

/// Writes records with rising rkeys, so later ones are newer.
struct W {
    t: u64,
}

impl W {
    fn next(&mut self) -> String {
        self.t += 1;
        tid(self.t)
    }

    /// `owner`'s list `l` holding `members`; a mod list when `modlist`.
    fn list(&mut self, g: &mut Graph, owner: &str, l: u32, members: &[&str], modlist: bool) -> String {
        let lu = list(owner, l);
        for m in members {
            let k = self.next();
            g.add_rec(&format!("at://{owner}/app.bsky.graph.listitem/{k}"), m, Some(&lu), 0).unwrap();
        }
        if modlist {
            g.add(&lu, &lu).unwrap();
        }
        lu
    }

    /// `s` subscribes to `l`; the record's URI.
    fn sub(&mut self, g: &mut Graph, s: &str, l: &str) -> String {
        let u = format!("at://{s}/app.bsky.graph.listblock/{}", self.next());
        g.add(&u, l).unwrap();
        u
    }
}

fn via(lists: ViaLists, kind: bool, dir: Dir) -> ViaQuery {
    ViaQuery { lists, members: LIST, kind: kind.then_some(MODLIST), dir }
}

fn out(kind: bool) -> ViaQuery {
    via(ViaLists::Rel(LISTBLOCK), kind, Dir::Out)
}

fn inn(kind: bool) -> ViaQuery {
    via(ViaLists::Rel(LISTBLOCK), kind, Dir::In)
}

fn id(g: &Graph, s: &str) -> u32 {
    g.id_of(s).unwrap()
}

/// Per other and via: the answering list's URI.
fn ask(g: &Graph, actor: &str, others: &[&str], vias: &[ViaQuery]) -> (Vec<Vec<Option<String>>>, ViaTruncated) {
    let ids: Vec<Option<u32>> = others.iter().map(|o| g.id_of(o)).collect();
    let (rows, t) = g.read(|cx| g.relationships_via(cx, g.id_of(actor), &ids, vias)).unwrap();
    let rows = rows.into_iter().map(|r| r.into_iter().map(|l| l.and_then(|l| g.did_of(l))).collect()).collect();
    (rows, t)
}

/// Per pair: (a list a subscribes to holding b, the reverse), as URIs.
type Answers = Vec<(Option<String>, Option<String>)>;

fn pairs(g: &Graph, ps: &[(&str, &str)]) -> (Answers, ViaTruncated) {
    let ids: Vec<Option<(u32, u32)>> = ps.iter().map(|(a, b)| g.id_of(a).zip(g.id_of(b))).collect();
    let (rows, t) = g.read(|cx| g.block_pairs_via(cx, &ids, &out(true))).unwrap();
    let uri = |l: Option<u32>| l.and_then(|l| g.did_of(l));
    (rows.into_iter().map(|(x, y)| (uri(x), uri(y))).collect(), t)
}

/// V subscribes to a mod list and a curation list that both hold T, and
/// T to a mod list holding V. Out finds T on V's lists, In finds V on T's,
/// each the lowest-id list that answers, and a kind keeps only mod lists.
#[test]
fn both_directions_and_both_list_sources() {
    let (o, v, t, x, w) = (did(1), did(2), did(3), did(4), did(5));
    for snapshot_between in [false, true] {
        let mut g = graph();
        let mut wr = W { t: 0 };
        // the curation list first, so it has the lower id
        let cur = wr.list(&mut g, &o, 1, &[&t, &x], false);
        let ml = wr.list(&mut g, &o, 2, &[&t], true);
        let back = wr.list(&mut g, &t, 1, &[&v], true);
        wr.sub(&mut g, &v, &cur);
        wr.sub(&mut g, &v, &ml);
        wr.sub(&mut g, &t, &back);
        if snapshot_between {
            g = reopen(&g);
        }
        assert!(id(&g, &cur) < id(&g, &ml));
        let given = |ls: &[&str], kind| via(ViaLists::Given(sorted(&g, ls)), kind, Dir::Out);
        let vias = [
            out(true),
            out(false),
            inn(true),
            inn(false),
            given(&[&ml, &cur], true),
            given(&[&cur], false),
            given(&[], true),
        ];
        let (r, cut) = ask(&g, &v, &[&t, &x, &w, &o], &vias);
        let s = |l: &String| Some(l.clone());
        assert_eq!(r[0], [s(&ml), s(&cur), s(&back), s(&back), s(&ml), s(&cur), None], "T");
        assert_eq!(r[1], [None, s(&cur), None, None, None, s(&cur), None], "X is on the curation list only");
        assert!(r[2].iter().all(Option::is_none), "W is on no list");
        assert!(r[3].iter().all(Option::is_none), "O owns the lists but isn't on them");
        assert_eq!(cut, ViaTruncated::default());

        // the same pairs both ways
        let (p, _) = pairs(&g, &[(&v, &t), (&t, &v), (&v, &x), (&w, &v)]);
        assert_eq!(p, [(s(&ml), s(&back)), (s(&back), s(&ml)), (None, None), (None, None)]);

        // the actor's side: an unknown or hidden actor (None) answers nothing
        let (rows, _) = g.read(|cx| g.relationships_via(cx, None, &[g.id_of(&t)], &[out(true)])).unwrap();
        assert_eq!(rows, [[None]]);
    }
}

fn sorted(g: &Graph, ls: &[&str]) -> Vec<u32> {
    let mut v: Vec<u32> = ls.iter().filter_map(|l| g.id_of(l)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// A list stops answering when it stops being a mod list, when its owner is
/// taken down (and again once restored), and when the owner is deleted. A
/// subscription or an item gone takes its answer with it, over a snapshot.
#[test]
fn purpose_status_and_deltas() {
    let (o, v, t) = (did(1), did(2), did(3));
    let mut g = graph();
    let mut wr = W { t: 0 };
    let ml = wr.list(&mut g, &o, 1, &[&t], true);
    let sub = wr.sub(&mut g, &v, &ml);
    let mut g = reopen(&g);
    let ask1 = |g: &Graph, kind| ask(g, &v, &[&t], &[out(kind)]).0[0][0].clone();
    assert_eq!(ask1(&g, true), Some(ml.clone()));

    // updated to a curation list: the modlist edge goes, the list still holds T
    g.delete(&ml).unwrap();
    assert_eq!(ask1(&g, true), None);
    assert_eq!(ask1(&g, false), Some(ml.clone()));
    g.add_at(&ml, &ml, i64::MAX / 2).unwrap();
    assert_eq!(ask1(&g, true), Some(ml.clone()), "and back");

    g.set_status(&o, Some(Status::Takendown), 0).unwrap();
    assert_eq!(ask1(&g, true), None, "a taken-down owner's list");
    assert_eq!(ask1(&g, false), None, "with no kind too: a hidden list answers nothing");
    let g2 = reopen(&g);
    assert_eq!(ask1(&g2, true), None, "through a snapshot");
    g.set_status(&o, None, 0).unwrap();
    assert_eq!(ask1(&g, true), Some(ml.clone()), "restored");

    g.delete(&sub).unwrap();
    assert_eq!(ask1(&g, true), None, "unsubscribed");
    wr.sub(&mut g, &v, &ml);
    assert_eq!(ask1(&g, true), Some(ml.clone()), "subscribed again, in the delta");

    g.set_status(&o, Some(Status::Deleted), 10).unwrap();
    assert_eq!(ask1(&g, true), None, "a deleted owner's list");
    assert_eq!(ask1(&reopen(&g), false), None);
}

/// The request actor's newest 1,000 subscriptions, each other's newest 100:
/// a list subscribed to before those isn't read, and the cut read counts.
/// A member on no list is never read for, and each subscriber is read once.
#[test]
fn caps_truncation_and_one_read_each() {
    let (o, v, t, u) = (did(1), did(2), did(3), did(4));
    let mut g = graph();
    let mut wr = W { t: 0 };
    let old = wr.list(&mut g, &o, 0, &[&t, &v], true);
    wr.sub(&mut g, &v, &old);
    wr.sub(&mut g, &t, &old);
    // filler: lists with members, subscribed to after `old`
    let n = VIA_ACTOR_CAP as u32 + 200;
    for l in 1..=n {
        let lu = wr.list(&mut g, &o, l, &[&u], false);
        wr.sub(&mut g, &v, &lu);
        if l <= VIA_OTHER_CAP as u32 + 10 {
            wr.sub(&mut g, &t, &lu);
        }
    }
    for snapshot_between in [false, true] {
        let g = if snapshot_between { reopen(&g) } else { g.clone() };
        let (r, cut) = ask(&g, &v, &[&t, &t, &u], &[out(true), inn(true)]);
        assert_eq!(r[0], [None, None], "both reads cut before `old`");
        assert_eq!(r[1], r[0]);
        assert_eq!(cut, ViaTruncated { out: 1, inn: 1 }, "V once, T once, though T is asked for twice");
        // U is on lists (the filler), but not on one V's newest 1,000 hold as a mod list
        assert_eq!(r[2], [None, None]);

        // O owns every list and is on none: no read at all, so nothing to cut
        let (_, cut) = ask(&g, &v, &[&o], &[out(true)]);
        assert_eq!(cut, ViaTruncated::default());

        // the pair (V, T) reads each side's newest 100, once over 30 pairs
        let ps: Vec<(&str, &str)> = (0..30).map(|_| (v.as_str(), t.as_str())).collect();
        let (p, cut) = pairs(&g, &ps);
        assert!(p.iter().all(|x| *x == (None, None)));
        assert_eq!(cut, ViaTruncated { out: 1, inn: 1 });
    }

    // subscribed again, `old` is the newest
    let mut g = g;
    wr.sub(&mut g, &v, &old);
    let (r, _) = ask(&g, &v, &[&t], &[out(true)]);
    assert_eq!(r[0][0], Some(old.clone()));
}

/// The actor named among the others is still the actor: In reads it with
/// the actor's cap, and Out and In share that one read.
#[test]
fn the_actor_as_an_other_keeps_its_cap() {
    let (o, v) = (did(1), did(2));
    let mut g = graph();
    let mut wr = W { t: 0 };
    let ml = wr.list(&mut g, &o, 0, &[&v], true);
    wr.sub(&mut g, &v, &ml);
    for l in 1..=VIA_OTHER_CAP as u32 + 50 {
        let lu = wr.list(&mut g, &o, l, &[&did(9)], false);
        wr.sub(&mut g, &v, &lu);
    }
    for g in [g.clone(), reopen(&g)] {
        let (r, cut) = ask(&g, &v, &[&v], &[inn(true), out(true)]);
        assert_eq!(r[0], [Some(ml.clone()), Some(ml.clone())], "ml is past V's newest 100, within its 1,000");
        assert_eq!(cut, ViaTruncated::default());
    }
}

/// A mass unsubscribe that no checkpoint has folded yet: the read still finds
/// the newest live subscriptions under a delta of deletes.
#[test]
fn a_large_delta_of_unsubscribes() {
    let (o, v, t) = (did(1), did(2), did(3));
    let mut g = graph();
    let mut wr = W { t: 0 };
    let ml = wr.list(&mut g, &o, 0, &[&t], true);
    wr.sub(&mut g, &v, &ml);
    let mut subs = Vec::new();
    for l in 1..=5000 {
        let lu = wr.list(&mut g, &o, l, &[&did(10)], true);
        subs.push(wr.sub(&mut g, &v, &lu));
    }
    let mut g = reopen(&g);
    let (r, cut) = ask(&g, &v, &[&t], &[out(true)]);
    assert_eq!((r[0][0].clone(), cut.out), (None, 1), "the newest 1,000 hold 5,000's filler");
    for s in &subs {
        g.delete(s).unwrap();
    }
    let (r, cut) = ask(&g, &v, &[&t], &[out(true)]);
    assert_eq!((r[0][0].clone(), cut.out), (Some(ml.clone()), 0), "past 5,000 deletes in the delta");

    // as an other, V's read passes at most 1,000 deletes: it stops short of
    // ml and counts as cut, until a snapshot folds the deletes
    assert_eq!(VIA_OTHER_CAP * VIA_MASKED_PER_RECORD, 1000);
    let (r, cut) = ask(&g, &t, &[&v], &[inn(true)]);
    assert_eq!((r[0][0].clone(), cut.inn), (None, 1), "5,000 deletes is past an other's 1,000");
    let p = g.read(|cx| g.via_subscriptions(cx, LISTBLOCK, id(&g, &v), VIA_OTHER_CAP)).unwrap();
    assert_eq!(p, (vec![], true));
    let (r, cut) = ask(&reopen(&g), &t, &[&v], &[inn(true)]);
    assert_eq!((r[0][0].clone(), cut.inn), (Some(ml.clone()), 0), "folded");
}
