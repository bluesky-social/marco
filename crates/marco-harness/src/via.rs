//! Via's cost: a synthetic snapshot with list subscriptions and list
//! purpose (`via-synth`), and latency per request shape on any snapshot that
//! has them (`via-bench`), the adversarial accounts and an uncheckpointed
//! mass unsubscribe included.
//!
//! The bench calls the Graph handlers' read functions, so a request costs
//! what the server's does short of the codec and HTTP: names to ids, the
//! direct relations, the vias, URIs out. Every page is loaded first when the
//! budget (MARCO_MEMORY) holds the snapshot, so the numbers are CPU on the
//! hit path, as on a server whose snapshot fits its memory.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use marco_core::graph::VIA_MAX_RECORDS;
use marco_core::{Graph, PairQuery, RelId, Relations, Status, ViaCuts};
use marco_proto::v1::__buffa::oneof::via;
use marco_proto::v1::*;
use marco_server::graph_svc::{self, ViaArgs};
use marco_server::synth::{did, tid};
use serde_json::{json, Value};

use crate::hist::Hist;
use crate::Rng;

/// The defaults, then a list subscription and a list's purpose.
pub const RELATIONS: &str = r#"
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

// ------------------------------------------------------------------ synth

#[derive(clap::Args)]
pub struct SynthArgs {
    /// The snapshot to write.
    #[arg(long)]
    pub out: PathBuf,
    /// Ordinary actors; lists are a tenth as many.
    #[arg(long, default_value_t = 100_000)]
    pub actors: u32,
    /// The heavy subscriber's listblock records, past 10 of real mod lists:
    /// lists nobody else names.
    #[arg(long, default_value_t = 200_000)]
    pub heavy_subscriptions: u32,
    /// Lists the hot member is on.
    #[arg(long, default_value_t = 50_000)]
    pub hot_member_lists: u32,
    /// Listed actors that also subscribe to more lists than a via reads
    /// (1,200 each): the worst case's population.
    #[arg(long, default_value_t = 2_000)]
    pub worst_subscribers: u32,
    #[arg(long, default_value_t = 7)]
    pub seed: u64,
}

/// A graph shaped like the network's lists, small. A fifth of the actors
/// can be listed; lists (a tenth as many as actors) have long-tailed sizes
/// and three in ten are mod lists, one has 50,000 members; 5% of actors
/// subscribe to a few lists, mostly mod lists, and an eighth of the listable
/// to 100-400; `worst_subscribers` of the listable to 1,200, past the
/// default cap; one more actor holds `heavy_subscriptions` subscriptions,
/// another is on `hot_member_lists` lists; 0.2% are taken down.
pub fn synth(a: &SynthArgs) -> Result<Value> {
    if a.out.exists() {
        bail!("{} exists", a.out.display());
    }
    let t0 = Instant::now();
    let mut g = Graph::new(Relations::parse_toml(RELATIONS)?);
    let mut rng = Rng(a.seed | 1);
    let n = a.actors.max(100);
    let mut t = 0u64;
    let mut key = move || {
        t += 1;
        tid(t, 0)
    };
    let uri = |actor: &str, coll: &str, rkey: &str| format!("at://{actor}/{coll}/{rkey}");
    let (mut follows, mut items, mut subs) = (0u64, 0u64, 0u64);
    for i in 0..n {
        let d = did(i);
        for _ in 0..200 / (1 + rng.below(100)) {
            let s = rng.below(n as u64) as u32;
            if s != i {
                g.add(&uri(&d, "app.bsky.graph.follow", &key()), &did(s))?;
                follows += 1;
            }
        }
        if rng.below(20) == 0 {
            g.add(&uri(&d, "app.bsky.graph.block", &key()), &did(rng.below(n as u64) as u32))?;
        }
    }
    let listable: Vec<bool> = (0..n).map(|_| rng.below(5) == 0).collect();
    let pool: Vec<u32> = (0..n).filter(|&i| listable[i as usize]).collect();
    let pick = |rng: &mut Rng| did(pool[rng.below(pool.len() as u64) as usize]);
    let (mut lists, mut modlists) = (Vec::new(), Vec::new());
    let mut list = |g: &mut Graph, rng: &mut Rng, owner: &str, l: String, members: &[String]| -> Result<()> {
        let lu = format!("at://{owner}/app.bsky.graph.list/{l}");
        for m in members {
            g.add_rec(&uri(owner, "app.bsky.graph.listitem", &key()), m, Some(&lu), 0)?;
        }
        if rng.below(10) < 3 {
            g.add(&lu, &lu)?;
            modlists.push(lu.clone());
        }
        lists.push(lu);
        Ok(())
    };
    for l in 0..n / 10 {
        let owner = did(rng.below(n as u64) as u32);
        let size = if l == 0 { 50_000.min(pool.len() as u64) } else { 1 + 2000 / (1 + rng.below(2000)) };
        let members: Vec<String> = (0..size).map(|_| pick(&mut rng)).collect();
        items += members.len() as u64;
        list(&mut g, &mut rng, &owner, format!("l{l}"), &members)?;
    }
    let hot = did(n + 1);
    for l in 0..a.hot_member_lists {
        let owner = did(rng.below(n as u64) as u32);
        items += 2;
        let members = [hot.clone(), pick(&mut rng)];
        list(&mut g, &mut rng, &owner, format!("h{l}"), &members)?;
    }
    if modlists.is_empty() {
        bail!("no mod lists: more actors");
    }
    let target = |rng: &mut Rng| {
        if rng.below(5) < 4 {
            modlists[rng.below(modlists.len() as u64) as usize].clone()
        } else {
            lists[rng.below(lists.len() as u64) as usize].clone()
        }
    };
    let mut sub = |g: &mut Graph, actor: &str, l: &str| g.add(&uri(actor, "app.bsky.graph.listblock", &key()), l);
    // spread over the listable, as many as there are
    let worst_n = (a.worst_subscribers as usize).min(pool.len());
    let worst: std::collections::BTreeSet<u32> = (0..worst_n).map(|k| pool[k * pool.len() / worst_n]).collect();
    for i in 0..n {
        let k = match rng.below(1000) {
            _ if worst.contains(&i) => VIA_MAX_RECORDS as u64 + 200,
            0..125 if listable[i as usize] => 100 + rng.below(300),
            0..50 => 1 + rng.below(5),
            _ => 0,
        };
        let d = did(i);
        for _ in 0..k {
            sub(&mut g, &d, &target(&mut rng))?;
        }
        subs += k;
    }
    // the heavy subscriber, on a list itself: real mod lists first, then lists nobody else names
    let heavy = did(n);
    let junk = did(n + 2);
    for _ in 0..10 {
        sub(&mut g, &heavy, &target(&mut rng))?;
    }
    for j in 0..a.heavy_subscriptions {
        sub(&mut g, &heavy, &format!("at://{junk}/app.bsky.graph.list/j{j}"))?;
    }
    subs += 10 + a.heavy_subscriptions as u64;
    let on = format!("at://{}/app.bsky.graph.list/onheavy", did(0));
    g.add_rec(&uri(&did(0), "app.bsky.graph.listitem", &key()), &heavy, Some(&on), 0)?;
    // every worst subscriber on a list nobody subscribes to, and never taken down, so each one in a
    // pair is read in full
    let wl = format!("at://{}/app.bsky.graph.list/worst", did(0));
    for &w in &worst {
        g.add_rec(&uri(&did(0), "app.bsky.graph.listitem", &key()), &did(w), Some(&wl), 0)?;
        items += 1;
    }
    let mut down = 0;
    for i in 0..n {
        if rng.below(500) == 0 && !worst.contains(&i) {
            g.set_status(&did(i), Some(Status::Takendown), 0)?;
            down += 1;
        }
    }
    let built = t0.elapsed().as_secs_f64();
    g.write_snapshot(&a.out)?;
    Ok(json!({
        "out": a.out.display().to_string(), "actors": n, "ids": g.n_ids(), "follows": follows, "listitems": items,
        "lists": lists.len(), "modlists": modlists.len(), "listblocks": subs, "worst_subscribers": worst.len(),
        "taken_down": down, "heavy_subscriber": heavy, "hot_member": hot, "build_secs": built,
        "total_secs": t0.elapsed().as_secs_f64(),
    }))
}

// ------------------------------------------------------------------ bench

#[derive(clap::Args)]
pub struct BenchArgs {
    /// A snapshot with the subscription, list and kind relations.
    #[arg(long)]
    pub snapshot: PathBuf,
    /// Requests per shape (BlockPairs of 1,000: at most 500).
    #[arg(long, default_value_t = 2000)]
    pub n: usize,
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    /// The most of a subscriber's records a via reads, as `marco serve --via-max-records`.
    #[arg(long, default_value_t = VIA_MAX_RECORDS)]
    pub cap: usize,
    /// Unsubscribes applied to the heaviest subscriber's newest records, and
    /// left uncheckpointed, before the delta shapes (0: none).
    #[arg(long, default_value_t = 150_000)]
    pub delta: usize,
    /// The relation whose subjects are the lists an actor subscribes to.
    #[arg(long, default_value = "listblock")]
    pub subscriptions: String,
    /// The list relation.
    #[arg(long, default_value = "listitem")]
    pub members: String,
    /// The relation that marks a list's kind.
    #[arg(long, default_value = "modlist")]
    pub kind: String,
    /// The JSON report (also printed).
    #[arg(long)]
    pub out: PathBuf,
}

/// `f` of every id, on every core.
fn par_ids<T: Send + Default + Clone>(n: u32, f: impl Fn(u32) -> T + Sync) -> Vec<T> {
    let parts = std::thread::available_parallelism().map_or(8, |p| p.get()) as u32;
    let step = n.div_ceil(parts).max(1);
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..parts)
            .map(|p| {
                let f = &f;
                s.spawn(move || (p * step..((p + 1) * step).min(n)).map(f).collect::<Vec<T>>())
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    })
}

/// The `k` ids of `ids` with the highest positive `count`, highest first.
fn top(ids: &[u32], k: usize, count: impl Fn(u32) -> u64) -> Vec<(u32, u64)> {
    let mut v: Vec<(u32, u64)> = ids.iter().map(|&i| (i, count(i))).filter(|x| x.1 > 0).collect();
    v.sort_unstable_by_key(|e| std::cmp::Reverse(e.1));
    v.truncate(k);
    v
}

/// `k` draws from `pool`, without repeats until it's used up (partial
/// shuffles).
fn distinct(rng: &mut Rng, pool: &[u32], k: usize) -> Vec<u32> {
    let mut p = pool.to_vec();
    let mut out = Vec::with_capacity(k);
    while out.len() < k {
        for j in 0..(k - out.len()).min(p.len()) {
            let r = j + rng.below((p.len() - j) as u64) as usize;
            p.swap(j, r);
            out.push(p[j]);
        }
    }
    out
}

fn rel_via(rel: &str, members: &str, kind: &str, dir: Dir) -> Via {
    Via {
        source: Some(via::Source::Rel(rel.into())),
        members: members.into(),
        kind: kind.into(),
        dir: dir.into(),
        ..Default::default()
    }
}

fn lists_via(lists: Vec<String>, members: &str, kind: &str) -> Via {
    Via {
        source: Some(ViaLists { uris: lists, ..Default::default() }.into()),
        ..rel_via("", members, kind, Dir::DIR_OUT)
    }
}

type RelShape = (&'static str, Vec<RelationshipsRequest>);
type PairShape = (&'static str, Vec<BlockPairsRequest>);

fn cut(c: ViaCuts) -> u64 {
    c.cap + c.deletes
}

/// Every shape's latency and the via reads the cap cut, keyed `prefix` + name.
fn run_shapes(
    g: &Graph,
    n: usize,
    cap: usize,
    prefix: &str,
    rel_shapes: &[RelShape],
    pair_shapes: &[PairShape],
    out: &mut BTreeMap<String, Value>,
) -> Result<()> {
    let err = |e: connectrpc::ConnectError| anyhow!("{e}");
    for (name, reqs) in rel_shapes {
        let reqs: Vec<(Vec<RelId>, Vec<ViaArgs>, &RelationshipsRequest)> = reqs
            .iter()
            .map(|r| {
                // as the handler resolves them: no rels is the relations between actors
                let rels = if r.rels.is_empty() {
                    g.relations().actor_rels()
                } else {
                    graph_svc::rel_ids(g, &r.rels).map_err(err)?
                };
                Ok((rels, graph_svc::vias_of(g, &r.via).map_err(err)?, r))
            })
            .collect::<Result<_>>()?;
        let (h, cuts, hits) = time(n, |i| {
            let (rels, vias, req) = &reqs[i % reqs.len()];
            let (r, c) = g.read(|cx| graph_svc::relationships_(g, cx, rels, vias, cap, req)).unwrap();
            let lists = r.relationships.iter().flat_map(|x| &x.via).filter(|a| !a.list.is_empty()).count();
            (cut(c), lists)
        });
        out.insert(format!("{prefix}{name}"), json!({"lat": h.summary(), "cut_reads": cuts, "via_hits": hits}));
    }
    for (name, reqs) in pair_shapes {
        let rels = graph_svc::block_pair_rels(g, &[]).map_err(err)?;
        let reqs: Vec<(Option<PairQuery>, &BlockPairsRequest)> = reqs
            .iter()
            .map(|r| Ok((r.via.as_option().map(|v| graph_svc::pair_via_of(g, v)).transpose().map_err(err)?, r)))
            .collect::<Result<_>>()?;
        let n = if name.contains("1000") { n.min(500) } else { n };
        let (h, cuts, hits) = time(n, |i| {
            let (via, req) = &reqs[i % reqs.len()];
            let (r, c) = g.read(|cx| graph_svc::block_pairs_(g, cx, &rels, via.as_ref(), cap, req)).unwrap();
            (cut(c), r.blocked.iter().filter(|&&b| b).count())
        });
        out.insert(format!("{prefix}{name}"), json!({"lat": h.summary(), "cut_reads": cuts, "blocked": hits}));
    }
    Ok(())
}

/// `n` calls after a tenth as many to warm up: their latency, and the sums
/// of what they returned (cut reads, hits).
fn time(n: usize, call: impl Fn(usize) -> (u64, usize)) -> (Hist, u64, usize) {
    for i in 0..n / 10 {
        std::hint::black_box(call(i));
    }
    let (mut h, mut cut, mut hits) = (Hist::default(), 0, 0);
    for i in 0..n {
        let t = Instant::now();
        let (c, x) = call(i);
        h.record(t.elapsed().as_nanos() as u64);
        cut += c;
        hits += x;
    }
    (h, cut, hits)
}

/// An id's counts, for drawing the pools: a list node's members, an actor's
/// subscriptions, the lists it's on and its follows.
#[derive(Clone, Default)]
struct Counts {
    subs: u64,
    on: u64,
    follows: u64,
    members: u64,
    node: bool,
}

pub fn bench(a: &BenchArgs) -> Result<Value> {
    if a.cap == 0 {
        bail!("--cap must be at least 1");
    }
    let t0 = Instant::now();
    let mut g = Graph::open(&a.snapshot)?;
    let open_secs = t0.elapsed().as_secs_f64();
    let t = Instant::now();
    let loaded = g.snapshot().is_some_and(|s| s.load());
    let load_secs = t.elapsed().as_secs_f64();
    let rels = g.relations().clone();
    let id = |n: &str| rels.by_name(n).with_context(|| format!("no relation {n:?}"));
    let (sub_r, mem_r, follow) = (id(&a.subscriptions)?, id(&a.members)?, id("follow")?);
    id(&a.kind)?;

    // pools, drawn once
    let t = Instant::now();
    let counts: Vec<Counts> = par_ids(g.n_ids(), |i| {
        let node = g.did_of(i).is_some_and(|d| d.starts_with("at://"));
        g.read(|cx| {
            Ok(if node {
                Counts { members: g.out_count(cx, mem_r, i)?, node, ..Default::default() }
            } else {
                Counts {
                    subs: g.out_count(cx, sub_r, i)?,
                    on: g.in_count(cx, mem_r, i)?,
                    follows: g.out_count(cx, follow, i)?,
                    ..Default::default()
                }
            })
        })
        .unwrap()
    });
    let pool = |f: &dyn Fn(&Counts) -> bool| -> Vec<u32> {
        (0..g.n_ids()).filter(|&i| !counts[i as usize].node && !g.hidden_id(i) && f(&counts[i as usize])).collect()
    };
    let viewers = pool(&|c| c.follows > 0);
    let actors = pool(&|_| true);
    let subscribers = pool(&|c| c.subs > 0);
    let listed = pool(&|c| c.on > 0);
    // the worst case: every actor on a list and holding more subscriptions than a read takes
    let worst = pool(&|c| c.subs as usize >= a.cap && c.on > 0);
    let lists: Vec<u32> = (0..g.n_ids()).filter(|&i| counts[i as usize].members > 0).collect();
    let heavy = top(&subscribers, 100, |i| counts[i as usize].subs);
    let hot = top(&listed, 10, |i| counts[i as usize].on);
    if viewers.is_empty() || listed.len() < 100 || subscribers.is_empty() || lists.is_empty() || heavy.is_empty() {
        bail!("the snapshot needs follows, 100 list members, subscriptions and lists");
    }
    let s = |i: u32| g.did_of(i).unwrap();
    let mut rng = Rng(a.seed | 1);
    let draw = |rng: &mut Rng, v: &[u32]| v[rng.below(v.len() as u64) as usize];
    const DRAWS: usize = 512;
    // (viewer, 100 others: half its follows where it has them, the rest any actor)
    let typical: Vec<(u32, Vec<u32>)> = (0..DRAWS)
        .map(|_| {
            let v = draw(&mut rng, &viewers);
            let f = g.read(|cx| g.out_set(cx, follow, v)).unwrap();
            let o = (0..100)
                .map(|k| if k % 2 == 0 && !f.is_empty() { draw(&mut rng, &f) } else { draw(&mut rng, &actors) })
                .collect();
            (v, o)
        })
        .collect();
    // a subscriber viewing 100 listed others
    let listed_reqs: Vec<(u32, Vec<u32>)> =
        (0..DRAWS).map(|_| (draw(&mut rng, &subscribers), distinct(&mut rng, &listed, 100))).collect();
    // a viewer's mute lists: none for most, a few for some, dozens for a few
    let mutes: Vec<Vec<String>> = (0..DRAWS)
        .map(|_| {
            let k = match rng.below(100) {
                0..80 => 0,
                80..95 => 1 + rng.below(3),
                _ => 10 + rng.below(40),
            };
            (0..k).map(|_| s(draw(&mut rng, &lists))).collect()
        })
        .collect();
    let pair = |a: u32, b: u32| BlockPair { a: s(a), b: s(b), ..Default::default() };
    let pairs_of = |rng: &mut Rng, k: usize| -> Vec<BlockPair> {
        (0..k).map(|_| pair(draw(rng, &viewers), draw(rng, &actors))).collect()
    };
    let pairs10: Vec<Vec<BlockPair>> = (0..DRAWS).map(|_| pairs_of(&mut rng, 10)).collect();
    let pairs100: Vec<Vec<BlockPair>> = (0..DRAWS).map(|_| pairs_of(&mut rng, 100)).collect();
    // the worst case's 2,000 accounts, distinct as far as the pool goes; without two, no worst case
    let worst_pool = if worst.len() >= 2 { &worst } else { &listed };
    let pairs1000: Vec<Vec<BlockPair>> =
        (0..64).map(|_| distinct(&mut rng, worst_pool, 2000).chunks(2).map(|c| pair(c[0], c[1])).collect()).collect();
    let (sub_n, mem_n, kind_n) = (a.subscriptions.as_str(), a.members.as_str(), a.kind.as_str());
    let rel_req = |actor: u32, others: &[u32], m: &[String], via: bool| {
        let mut vias = vec![rel_via(sub_n, mem_n, kind_n, Dir::DIR_OUT), rel_via(sub_n, mem_n, kind_n, Dir::DIR_IN)];
        if !m.is_empty() {
            vias.push(lists_via(m.to_vec(), mem_n, kind_n));
        }
        RelationshipsRequest {
            actor: s(actor),
            others: others.iter().map(|&o| s(o)).collect(),
            rels: vec!["follow".into(), "block".into()],
            via: if via { vias } else { vec![] },
            ..Default::default()
        }
    };
    let pair_req = |pairs: &[BlockPair], via: bool| BlockPairsRequest {
        pairs: pairs.to_vec(),
        via: if via {
            PairVia { rel: sub_n.into(), members: mem_n.into(), kind: kind_n.into(), ..Default::default() }.into()
        } else {
            Default::default()
        },
        ..Default::default()
    };
    let (hv, hv_n) = heavy[0];
    let heavy_ids: Vec<u32> = heavy.iter().map(|h| h.0).collect();
    let hot_ids: Vec<u32> = hot.iter().map(|h| h.0).collect();
    let rel_shapes: Vec<RelShape> = vec![
        ("rel_100x2", typical.iter().zip(&mutes).map(|((v, o), m)| rel_req(*v, o, m, false)).collect()),
        ("rel_100x2_via", typical.iter().zip(&mutes).map(|((v, o), m)| rel_req(*v, o, m, true)).collect()),
        ("rel_100x2_via_listed", listed_reqs.iter().zip(&mutes).map(|((v, o), m)| rel_req(*v, o, m, true)).collect()),
        ("rel_100x2_via_heavy_viewer", listed_reqs.iter().map(|(_, o)| rel_req(hv, o, &[], true)).collect()),
        (
            "rel_100x2_via_heavy_others",
            (0..DRAWS)
                .map(|i| {
                    let o: Vec<u32> =
                        heavy_ids.iter().copied().cycle().skip(i).take(heavy_ids.len().min(100)).collect();
                    rel_req(listed[i % listed.len()], &o, &[], true)
                })
                .collect(),
        ),
        (
            "rel_100x2_via_hot_members",
            listed_reqs
                .iter()
                .map(|(v, o)| {
                    let mut o = o.clone();
                    o[..hot_ids.len()].copy_from_slice(&hot_ids);
                    rel_req(*v, &o, &[], true)
                })
                .collect(),
        ),
    ];
    let mut pair_shapes: Vec<PairShape> = vec![
        ("pairs_10", pairs10.iter().map(|p| pair_req(p, false)).collect()),
        ("pairs_10_via", pairs10.iter().map(|p| pair_req(p, true)).collect()),
        ("pairs_100_via", pairs100.iter().map(|p| pair_req(p, true)).collect()),
        ("pairs_1000", pairs1000.iter().map(|p| pair_req(p, false)).collect()),
    ];
    if worst.len() >= 2 {
        pair_shapes.push(("pairs_1000_via_worst", pairs1000.iter().map(|p| pair_req(p, true)).collect()));
    }
    // after the mass unsubscribe: the heavy subscriber as viewer, as an other, as a pair's a
    let delta_rel: Vec<RelShape> = vec![
        ("rel_100x2_via_heavy_viewer", rel_shapes[3].1.clone()),
        (
            "rel_100x2_via_heavy_other",
            listed_reqs
                .iter()
                .enumerate()
                .map(|(i, (_, o))| {
                    let mut o = o.clone();
                    o[0] = hv;
                    rel_req(listed[(i + 1) % listed.len()], &o, &[], true)
                })
                .collect(),
        ),
    ];
    let delta_pairs: Vec<PairShape> = vec![(
        "pairs_100_via_heavy",
        pairs100
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut p = p.clone();
                p[0] = pair(hv, listed[i % listed.len()]);
                pair_req(&p, true)
            })
            .collect(),
    )];
    let page = g.read(|cx| g.out_page(cx, sub_r, hv, a.delta, None)).unwrap();
    let unsubscribes: Vec<String> = page.iter().filter_map(|&(k, _)| g.uri(sub_r, hv, k)).collect();
    let inputs_secs = t.elapsed().as_secs_f64();

    let mut reads = BTreeMap::new();
    run_shapes(&g, a.n, a.cap, "", &rel_shapes, &pair_shapes, &mut reads)?;
    let t = Instant::now();
    g.quiet = true;
    for u in &unsubscribes {
        g.delete(u)?;
    }
    let delta_secs = t.elapsed().as_secs_f64();
    if !unsubscribes.is_empty() {
        run_shapes(&g, a.n, a.cap, "delta_", &delta_rel, &delta_pairs, &mut reads)?;
    }
    let report = json!({
        "snapshot": a.snapshot.display().to_string(), "cap": a.cap,
        "open_secs": open_secs, "loaded": loaded, "load_secs": load_secs, "inputs_secs": inputs_secs,
        "inputs": {
            "ids": g.n_ids(), "viewers": viewers.len(), "subscribers": subscribers.len(), "listed": listed.len(),
            "worst_pool": worst.len(), "worst_distinct_per_request": worst.len().min(2000),
            "lists_with_members": lists.len(),
            "heaviest_subscriptions": heavy.iter().take(5).map(|h| h.1).collect::<Vec<_>>(),
            "hottest_member_lists": hot.iter().take(5).map(|h| h.1).collect::<Vec<_>>(),
        },
        "delta": {"subscriber_records": hv_n, "unsubscribes": unsubscribes.len(), "apply_secs": delta_secs},
        "shapes": reads,
    });
    std::fs::write(&a.out, serde_json::to_vec_pretty(&report)?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bench runs every shape on a small synthetic snapshot, and the
    /// heavy subscriber's mass unsubscribe leaves its reads cut, not slow.
    #[test]
    fn synth_then_bench() {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("via.marco");
        let args = |heavy, hot, worst| SynthArgs {
            out: snap.clone(),
            actors: 3000,
            heavy_subscriptions: heavy,
            hot_member_lists: hot,
            worst_subscribers: worst,
            seed: 3,
        };
        let s = synth(&args(5000, 500, 20)).unwrap();
        assert!(s["modlists"].as_u64().unwrap() > 0 && s["listblocks"].as_u64().unwrap() > 5000);
        assert_eq!(s["worst_subscribers"], 20);
        assert!(synth(&args(0, 0, 0)).is_err(), "an existing snapshot isn't overwritten");
        let a = BenchArgs {
            snapshot: snap,
            n: 20,
            seed: 1,
            cap: VIA_MAX_RECORDS,
            delta: 4000,
            subscriptions: "listblock".into(),
            members: "listitem".into(),
            kind: "modlist".into(),
            out: dir.path().join("bench.json"),
        };
        let r = bench(&a).unwrap();
        let shapes = r["shapes"].as_object().unwrap();
        assert_eq!(shapes.len(), 11 + 3, "{:?}", shapes.keys().collect::<Vec<_>>());
        assert!(shapes.values().all(|v| v["lat"]["n"].as_u64() == Some(20)));
        assert_eq!(r["delta"]["unsubscribes"], 4000);
        // every worst subscriber is listed, visible and past the cap, and so is the heavy one
        assert_eq!(r["inputs"]["worst_pool"], 21);
        assert!(shapes["rel_100x2_via_listed"]["via_hits"].as_u64().unwrap() > 0);
        assert_eq!(shapes["rel_100x2"]["cut_reads"], 0, "no via, no reads");
        // the heavy viewer's read, once a request: cut at the cap before the delta, at its masked budget after
        assert!(shapes["rel_100x2_via_heavy_viewer"]["cut_reads"].as_u64().unwrap() >= 20);
        assert!(shapes["delta_rel_100x2_via_heavy_viewer"]["cut_reads"].as_u64().unwrap() >= 20);
        assert!(std::fs::metadata(dir.path().join("bench.json")).unwrap().len() > 0);
    }
}
