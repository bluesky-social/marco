use std::time::{Duration, Instant};

use marco_core::graph::AmongEdge;
use marco_core::{Among, AmongPlan, Graph, Relations};
use vlatproto::tid::Tid;

const FOLLOW: u8 = 0;
const BLOCK: u8 = 1;
const PLANS: [AmongPlan; 5] =
    [AmongPlan::Auto, AmongPlan::Scan, AmongPlan::Probe, AmongPlan::Cost(1), AmongPlan::Cost(4)];

fn did(i: u32) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut v = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
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

fn uri(rel: u8, a: u32, t: u64) -> String {
    let coll = if rel == FOLLOW { "app.bsky.graph.follow" } else { "app.bsky.graph.block" };
    format!("at://{}/{coll}/{}", did(a), tid(t))
}

fn reopen(g: &Graph) -> Graph {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("g.marco");
    g.write_snapshot(&p).unwrap();
    let mut g2 = Graph::open(&p).unwrap();
    g2.shards = g.shards;
    g2
}

fn ids(g: &Graph, members: &[u32]) -> Vec<u32> {
    let mut v: Vec<u32> = members.iter().filter_map(|&m| g.id_of(&did(m))).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn among(g: &Graph, members: &[u32], rels: &[u8], limit: usize, plan: AmongPlan) -> Among {
    let ids = ids(g, members);
    g.read(|cx| g.edges_among(cx, &ids, rels, limit, plan, None)).unwrap().unwrap()
}

/// Every pair asked with has_id: the answer EdgesAmong must give.
fn brute(g: &Graph, members: &[u32], rels: &[u8]) -> (Vec<AmongEdge>, Vec<u64>) {
    let ids = ids(g, members);
    let mut edges = Vec::new();
    let mut counts = vec![0; rels.len()];
    for (ri, &rel) in rels.iter().enumerate() {
        for &a in &ids {
            for &s in &ids {
                if let Some(k) = g.read(|cx| g.has_id(cx, rel, a, s)).unwrap() {
                    edges.push((rel, a, s, k));
                    counts[ri] += 1;
                }
            }
        }
    }
    (edges, counts)
}

fn check(g: &Graph, members: &[u32], rels: &[u8], what: &str) {
    let (want, counts) = brute(g, members, rels);
    for plan in PLANS {
        let r = among(g, members, rels, usize::MAX, plan);
        assert_eq!(r.edges, want, "{what} {plan:?}");
        assert_eq!(r.counts, counts, "{what} {plan:?}");
        assert!(!r.truncated);
        for limit in [0, 1, want.len() / 2, want.len()] {
            let r = among(g, members, rels, limit, plan);
            assert_eq!(r.edges, want[..limit.min(want.len())], "{what} {plan:?} limit {limit}");
            assert_eq!(r.counts, counts, "{what} {plan:?} limit {limit}");
            assert_eq!(r.truncated, want.len() > limit, "{what} {plan:?} limit {limit}");
        }
    }
}

#[test]
fn small_graph_both_directions_deletes_and_repeats() {
    let mut g = Graph::new(Relations::bsky_default());
    g.shards = 3;
    // 1 and 2 follow each other, 1 follows 3 twice, 3 blocks 1, 4 follows 1 (4 is outside the set)
    g.add(&uri(FOLLOW, 1, 1), &did(2)).unwrap();
    g.add(&uri(FOLLOW, 2, 2), &did(1)).unwrap();
    g.add(&uri(FOLLOW, 1, 3), &did(3)).unwrap();
    g.add(&uri(FOLLOW, 1, 4), &did(3)).unwrap();
    g.add(&uri(BLOCK, 3, 5), &did(1)).unwrap();
    g.add(&uri(FOLLOW, 4, 6), &did(1)).unwrap();
    g.add(&uri(FOLLOW, 2, 7), &did(4)).unwrap();
    let set = [1, 2, 3];
    for (what, g) in [("live", g.clone()), ("reopened", reopen(&g))] {
        let r = among(&g, &set, &[FOLLOW, BLOCK], usize::MAX, AmongPlan::Auto);
        let id = |i| g.id_of(&did(i)).unwrap();
        let got: Vec<(u8, u32, u32)> = r.edges.iter().map(|e| (e.0, e.1, e.2)).collect();
        let mut want = vec![(FOLLOW, id(1), id(2)), (FOLLOW, id(2), id(1)), (FOLLOW, id(1), id(3))];
        want.sort_unstable();
        want.push((BLOCK, id(3), id(1)));
        assert_eq!(got, want, "{what}");
        assert_eq!(r.counts, vec![3, 1], "{what}");
        let k = r.edges.iter().find(|e| e.1 == id(1) && e.2 == id(3)).unwrap().3;
        assert_eq!(g.uri(FOLLOW, id(1), k), Some(uri(FOLLOW, 1, 4)), "{what}: the newest of two records");
        check(&g, &set, &[FOLLOW, BLOCK], what);
        check(&g, &[1, 2, 3, 4], &[BLOCK, FOLLOW], what);
    }
    // deletes, live and over a snapshot
    let mut g2 = reopen(&g);
    for g in [&mut g, &mut g2] {
        g.delete(&uri(FOLLOW, 1, 4)).unwrap();
        g.delete(&uri(FOLLOW, 2, 2)).unwrap();
        let r = among(g, &set, &[FOLLOW], usize::MAX, AmongPlan::Auto);
        assert_eq!(r.counts, vec![2]);
        let k = r.edges.iter().find(|e| e.2 == g.id_of(&did(3)).unwrap()).unwrap().3;
        assert_eq!(g.uri(FOLLOW, g.id_of(&did(1)).unwrap(), k), Some(uri(FOLLOW, 1, 3)));
        check(g, &set, &[FOLLOW, BLOCK], "deleted");
        g.delete(&uri(FOLLOW, 1, 3)).unwrap();
        g.delete(&uri(FOLLOW, 1, 1)).unwrap();
        assert_eq!(among(g, &set, &[FOLLOW], usize::MAX, AmongPlan::Probe).counts, vec![0]);
        check(g, &set, &[FOLLOW, BLOCK], "all deleted");
    }
}

#[test]
fn random_graphs_match_every_pair() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    for round in 0..4 {
        let mut g = Graph::new(Relations::bsky_default());
        g.shards = 4;
        g.set_subj_index(16);
        let actors = 80u64;
        let mut t = 0;
        let mut live = Vec::new();
        // a few heavy actors with long, split out logs (past one page's whole list)
        for heavy in 0..3 {
            for s in 0..actors + 200 {
                t += 1;
                let u = uri(FOLLOW, heavy, t);
                g.add(&u, &did(s as u32)).unwrap();
                live.push(u);
            }
        }
        for _ in 0..1500 {
            t += 1;
            let rel = if rnd(5) == 0 { BLOCK } else { FOLLOW };
            let (a, s) = (rnd(actors) as u32, rnd(actors) as u32);
            let u = uri(rel, a, t);
            g.add(&u, &did(s)).unwrap();
            live.push(u);
        }
        let mut gs = vec![("live", g.clone())];
        let mut base = reopen(&g);
        gs.push(("base", base.clone()));
        // deletes and adds over the base: deltas on base lists
        for _ in 0..300 {
            let i = rnd(live.len() as u64) as usize;
            base.delete(&live.swap_remove(i)).unwrap();
            t += 1;
            let (a, s) = (rnd(actors) as u32, rnd(actors) as u32);
            base.add(&uri(FOLLOW, a, t), &did(s)).unwrap();
        }
        gs.push(("base+delta", base));
        for (what, g) in &gs {
            for n in [1, 2, 5, 20, 60, 100] {
                let set: Vec<u32> = (0..n).map(|_| rnd(actors + 10) as u32).collect();
                check(g, &set, &[FOLLOW, BLOCK], &format!("round {round} {what} n {n}"));
            }
        }
    }
}

#[test]
fn a_passed_deadline_answers_nothing() {
    let mut g = Graph::new(Relations::bsky_default());
    g.add(&uri(FOLLOW, 1, 1), &did(2)).unwrap();
    let ids = ids(&g, &[1, 2]);
    let past = Instant::now() - Duration::from_millis(1);
    assert_eq!(g.read(|cx| g.edges_among(cx, &ids, &[FOLLOW], 10, AmongPlan::Auto, Some(past))).unwrap(), None);
    let later = Instant::now() + Duration::from_secs(60);
    let r = g.read(|cx| g.edges_among(cx, &ids, &[FOLLOW], 10, AmongPlan::Auto, Some(later))).unwrap().unwrap();
    assert_eq!(r.counts, vec![1]);
}
