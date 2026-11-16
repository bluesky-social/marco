//! Via: a list a subscriber is connected to that holds a member, both
//! directions, from a relation or from the caller's lists, over a snapshot
//! and over deltas.

use marco_core::graph::{ViaCut, VIA_MASKED_PER_RECORD, VIA_MAX_RECORDS};
use marco_core::{Dir, Graph, PairQuery, Relations, Status, ViaCuts, ViaHit, ViaLists, ViaQuery};
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

/// An answer as a URI, or "…" for an empty answer a cut read may have missed.
fn uri(g: &Graph, h: ViaHit) -> Option<String> {
    match h {
        ViaHit { list: Some(l), truncated: false } => g.did_of(l),
        ViaHit { list: None, truncated: true } => Some("…".into()),
        ViaHit { list: None, truncated: false } => None,
        h => panic!("a hit can't be truncated: {h:?}"),
    }
}

/// Per other and via: the answering list's URI.
fn ask(g: &Graph, actor: &str, others: &[&str], vias: &[ViaQuery], cap: usize) -> (Vec<Vec<Option<String>>>, ViaCuts) {
    let ids: Vec<Option<u32>> = others.iter().map(|o| g.id_of(o)).collect();
    let (rows, t) = g.read(|cx| g.relationships_via(cx, g.id_of(actor), &ids, vias, cap)).unwrap();
    (rows.into_iter().map(|r| r.into_iter().map(|h| uri(g, h)).collect()).collect(), t)
}

/// Per pair: (a list a subscribes to holding b, the reverse), as URIs.
type Answers = Vec<(Option<String>, Option<String>)>;

fn pairs(g: &Graph, ps: &[(&str, &str)], kind: bool, cap: usize) -> (Answers, ViaCuts) {
    let ids: Vec<Option<(u32, u32)>> = ps.iter().map(|(a, b)| g.id_of(a).zip(g.id_of(b))).collect();
    let q = PairQuery { rel: LISTBLOCK, members: LIST, kind: kind.then_some(MODLIST) };
    let (rows, t) = g.read(|cx| g.block_pairs_via(cx, &ids, &q, cap)).unwrap();
    (rows.into_iter().map(|(x, y)| (uri(g, x), uri(g, y))).collect(), t)
}

fn sorted(g: &Graph, ls: &[&str]) -> Vec<u32> {
    let mut v: Vec<u32> = ls.iter().filter_map(|l| g.id_of(l)).collect();
    v.sort_unstable();
    v.dedup();
    v
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
        let (r, cut) = ask(&g, &v, &[&t, &x, &w, &o], &vias, VIA_MAX_RECORDS);
        let s = |l: &String| Some(l.clone());
        assert_eq!(r[0], [s(&ml), s(&cur), s(&back), s(&back), s(&ml), s(&cur), None], "T");
        assert_eq!(r[1], [None, s(&cur), None, None, None, s(&cur), None], "X is on the curation list only");
        assert!(r[2].iter().all(Option::is_none), "W is on no list");
        assert!(r[3].iter().all(Option::is_none), "O owns the lists but isn't on them");
        assert_eq!(cut, ViaCuts::default());

        // the same pairs both ways
        let (p, _) = pairs(&g, &[(&v, &t), (&t, &v), (&v, &x), (&w, &v)], true, VIA_MAX_RECORDS);
        assert_eq!(p, [(s(&ml), s(&back)), (s(&back), s(&ml)), (None, None), (None, None)]);

        // the actor's side: an unknown or hidden actor (None) answers nothing
        let (rows, _) =
            g.read(|cx| g.relationships_via(cx, None, &[g.id_of(&t)], &[out(true)], VIA_MAX_RECORDS)).unwrap();
        assert_eq!(rows, [[ViaHit::default()]]);
    }
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
    let ask1 = |g: &Graph, kind| ask(g, &v, &[&t], &[out(kind)], VIA_MAX_RECORDS).0[0][0].clone();
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

/// Every subscriber's newest `cap` subscriptions, whichever side names it:
/// a list subscribed to before those isn't read, the empty answers it may
/// have missed say so, and the cut read counts once. A member on no list is
/// never read for, and an answer that found a list isn't truncated.
#[test]
fn the_cap_truncation_and_one_read_each() {
    const CAP: usize = 20;
    let (o, v, t, u) = (did(1), did(2), did(3), did(4));
    let mut g = graph();
    let mut wr = W { t: 0 };
    let old = wr.list(&mut g, &o, 0, &[&t, &v], true);
    wr.sub(&mut g, &v, &old);
    wr.sub(&mut g, &t, &old);
    // filler: lists with U on them, subscribed to after `old`; the last of V's holds T
    let n = CAP as u32 + 5;
    let mut last = String::new();
    for l in 1..=n {
        let members: &[&str] = if l == n { &[&u, &t] } else { &[&u] };
        last = wr.list(&mut g, &o, l, members, false);
        wr.sub(&mut g, &v, &last);
        wr.sub(&mut g, &t, &last);
    }
    let cut = ViaCuts { cap: 2, deletes: 0 };
    for snapshot_between in [false, true] {
        let g = if snapshot_between { reopen(&g) } else { g.clone() };
        let (r, c) = ask(&g, &v, &[&t, &t, &u], &[out(true), inn(true)], CAP);
        let trunc = Some("…".to_string());
        assert_eq!(r[0], [trunc.clone(), trunc.clone()], "both reads cut before `old`");
        assert_eq!(r[1], r[0]);
        assert_eq!(c, cut, "V once, T once, though T is asked for twice");
        // U is on V's newest lists, but none of them is a mod list; In: U has no subscriptions
        assert_eq!(r[2], [trunc.clone(), None]);
        // without a kind, the newest list holding T answers, and nothing is truncated
        let (r, _) = ask(&g, &v, &[&t], &[out(false)], CAP);
        assert_eq!(r[0], [Some(last.clone())]);

        // O owns every list and is on none: no read at all, so nothing to cut
        let (r, c) = ask(&g, &v, &[&o], &[out(true)], CAP);
        assert_eq!((r[0][0].clone(), c), (None, ViaCuts::default()));

        // a cap past every subscription finds `old` both ways
        let (r, c) = ask(&g, &v, &[&t], &[out(true), inn(true)], VIA_MAX_RECORDS);
        assert_eq!((r[0].clone(), c), (vec![Some(old.clone()), Some(old.clone())], ViaCuts::default()));

        // the pair (V, T) reads each side once over 30 pairs, with the same cap
        let ps: Vec<(&str, &str)> = (0..30).map(|_| (v.as_str(), t.as_str())).collect();
        let (p, c) = pairs(&g, &ps, true, CAP);
        assert!(p.iter().all(|x| *x == (trunc.clone(), trunc.clone())));
        assert_eq!(c, cut);
    }

    // subscribed again, `old` is the newest
    let mut g = g;
    wr.sub(&mut g, &v, &old);
    let (r, _) = ask(&g, &v, &[&t], &[out(true)], CAP);
    assert_eq!(r[0][0], Some(old.clone()));
}

/// One rule for every read of a subscriber: Relationships from either side
/// and BlockPairs give the same answer about the same pair, under a cap
/// that cuts one side's subscriptions.
#[test]
fn every_call_reads_a_subscriber_alike() {
    const CAP: usize = 8;
    let (o, v, t) = (did(1), did(2), did(3));
    let mut g = graph();
    let mut wr = W { t: 0 };
    // the mod list holding T is V's oldest subscription, inside the cap; the
    // one holding V is T's oldest, past it
    let vt = wr.list(&mut g, &o, 0, &[&t], true);
    let tv = wr.list(&mut g, &o, 1, &[&v], true);
    wr.sub(&mut g, &t, &tv);
    wr.sub(&mut g, &v, &vt);
    for l in 0..CAP as u32 {
        let f = wr.list(&mut g, &o, 10 + l, &[&did(9)], true);
        wr.sub(&mut g, &t, &f);
        if l + 3 < CAP as u32 {
            wr.sub(&mut g, &v, &f);
        }
    }
    for g in [g.clone(), reopen(&g)] {
        let s = |l: &String| Some(l.clone());
        let trunc = Some("…".to_string());
        let (from_v, _) = ask(&g, &v, &[&t], &[out(true), inn(true)], CAP);
        let (from_t, _) = ask(&g, &t, &[&v], &[inn(true), out(true)], CAP);
        let (p, _) = pairs(&g, &[(&v, &t), (&t, &v)], true, CAP);
        assert_eq!(from_v[0], [s(&vt), trunc.clone()], "V's lists hold T, T's past the cap");
        assert_eq!(from_t[0], from_v[0], "asked from T's side");
        assert_eq!(p, [(s(&vt), trunc.clone()), (trunc.clone(), s(&vt))], "and as pairs");
    }
}

/// A read that stops on its bound on deletes keeps to the newest records it
/// reached: a subscription in the delta older than where it stopped might
/// sit behind live records it never read, so it doesn't answer.
#[test]
fn a_cut_read_keeps_to_the_newest() {
    let (o, v, t) = (did(1), did(2), did(3));
    let mut g = graph();
    let mut wr = W { t: 100 };
    let a = wr.list(&mut g, &o, 0, &[&did(9)], true);
    wr.sub(&mut g, &v, &a);
    let newer: Vec<String> = (1..=11)
        .map(|l| {
            let f = wr.list(&mut g, &o, l, &[&did(9)], true);
            wr.sub(&mut g, &v, &f)
        })
        .collect();
    let mut g = reopen(&g);
    for s in &newer {
        g.delete(s).unwrap();
    }
    // written now with an older rkey than A's subscription: behind it in the log
    let b = wr.list(&mut g, &o, 50, &[&t], true);
    g.add(&format!("at://{v}/app.bsky.graph.listblock/{}", tid(1)), &b).unwrap();
    // a cap of 1 passes 10 deletes, stops at the 11th, short of A
    let (r, c) = ask(&g, &v, &[&t], &[out(true)], 1);
    assert_eq!((r[0][0].clone(), c), (Some("…".into()), ViaCuts { cap: 0, deletes: 1 }));
    // a cap of 10 passes them all, reads A and then B, which answers
    let (r, c) = ask(&g, &v, &[&t], &[out(true)], 10);
    assert_eq!((r[0][0].clone(), c), (Some(b.clone()), ViaCuts::default()));
}

/// A mass unsubscribe that no checkpoint has folded yet: the read still finds
/// the newest live subscriptions under a delta of deletes, up to a bound on
/// the deletes it passes, past which it stops and says so.
#[test]
fn a_large_delta_of_unsubscribes() {
    const CAP: usize = 50;
    let (o, v, t) = (did(1), did(2), did(3));
    let mut g = graph();
    let mut wr = W { t: 0 };
    let ml = wr.list(&mut g, &o, 0, &[&t], true);
    wr.sub(&mut g, &v, &ml);
    let mut subs = Vec::new();
    let n = (CAP * VIA_MASKED_PER_RECORD) as u32 + 100;
    for l in 1..=n {
        let lu = wr.list(&mut g, &o, l, &[&did(10)], true);
        subs.push(wr.sub(&mut g, &v, &lu));
    }
    let mut g = reopen(&g);
    let (r, c) = ask(&g, &v, &[&t], &[out(true)], CAP);
    assert_eq!((r[0][0].clone(), c), (Some("…".into()), ViaCuts { cap: 1, deletes: 0 }), "the newest 50 are filler");
    for s in &subs {
        g.delete(s).unwrap();
    }
    // 600 deletes since the checkpoint is past what a 50-record read passes
    let (r, c) = ask(&g, &v, &[&t], &[out(true)], CAP);
    assert_eq!((r[0][0].clone(), c), (Some("…".into()), ViaCuts { cap: 0, deletes: 1 }));
    let p = g.read(|cx| g.via_subscriptions(cx, LISTBLOCK, id(&g, &v), CAP)).unwrap();
    assert_eq!(p, (vec![], ViaCut::Deletes));
    // a larger cap passes more, and gets there
    let (r, c) = ask(&g, &v, &[&t], &[out(true)], VIA_MAX_RECORDS);
    assert_eq!((r[0][0].clone(), c), (Some(ml.clone()), ViaCuts::default()));
    // and once a checkpoint folds the deletes, so does the small one
    let (r, c) = ask(&reopen(&g), &v, &[&t], &[out(true)], CAP);
    assert_eq!((r[0][0].clone(), c), (Some(ml.clone()), ViaCuts::default()), "folded");
}
