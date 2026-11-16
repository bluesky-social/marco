//! EdgesAmong on a built snapshot: sets of 5 to 5,000 accounts drawn from
//! popular accounts, from ordinary ones, and from one account's followers
//! (a community), each answered by every plan.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use rayon::prelude::*;
use serde_json::{json, Value};

use marco_core::{AmongPlan, Graph, RelId};

use crate::stats::{summarize, Rng};

const FOLLOW: RelId = 0;
const BLOCK: RelId = 1;

#[derive(clap::Args)]
pub struct Args {
    #[arg(long)]
    snapshot: PathBuf,
    /// Set sizes.
    #[arg(long, value_delimiter = ',', default_value = "5,50,500,5000")]
    sizes: Vec<usize>,
    /// Sets per size and shape: this many actors' worth, 10 at least and at most --sets.
    #[arg(long, default_value_t = 20_000)]
    actors: usize,
    #[arg(long, default_value_t = 200)]
    sets: usize,
    /// Plans besides auto, scan and probe: probe-cost thresholds.
    #[arg(long, value_delimiter = ',', default_value = "16,32,128,256")]
    costs: Vec<u64>,
    /// The popular pool: this many accounts with the most followers.
    #[arg(long, default_value_t = 20_000)]
    top: usize,
    /// The edges a request returns (the handler's default).
    #[arg(long, default_value_t = 10_000)]
    limit: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
}

fn pick(r: &mut Rng, pool: &[u32], n: usize) -> Vec<u32> {
    let mut v: Vec<u32> = (0..n).map(|_| pool[r.below(pool.len() as u64) as usize]).collect();
    v.sort_unstable();
    v.dedup();
    v
}

pub fn run(a: &Args) -> Result<Value> {
    let t = Instant::now();
    let g = Graph::open(&a.snapshot)?;
    let store = g.snapshot().unwrap();
    let loaded = store.load();
    let mut out = BTreeMap::new();
    out.insert("open_and_load_secs".to_string(), json!(t.elapsed().as_secs_f64()));
    out.insert("loaded".into(), json!(loaded));

    let n_ids = g.n_ids();
    let t = Instant::now();
    let deg: Vec<(u64, u64)> = (0..n_ids)
        .into_par_iter()
        .map(|id| g.read(|cx| Ok((g.out_count(cx, FOLLOW, id)?, g.in_count(cx, FOLLOW, id)?))).unwrap())
        .collect();
    out.insert("degrees_secs".into(), json!(t.elapsed().as_secs_f64()));
    let mut by_in: Vec<u32> = (0..n_ids).filter(|&i| deg[i as usize].1 > 0).collect();
    by_in.sort_unstable_by_key(|&i| std::cmp::Reverse(deg[i as usize].1));
    let popular: Vec<u32> = by_in[..a.top.min(by_in.len())].to_vec();
    let ordinary: Vec<u32> =
        (0..n_ids).filter(|&i| (1..=500).contains(&deg[i as usize].0) && deg[i as usize].1 <= 500).collect();
    // hubs: accounts whose followers make communities of each size
    let hubs = |n: usize| -> Vec<u32> {
        let (lo, hi) = (2 * n as u64, (100 * n as u64).max(20_000));
        by_in.iter().copied().filter(|&i| (lo..=hi).contains(&deg[i as usize].1)).collect()
    };
    let pct = |v: &mut Vec<u64>, p: f64| {
        v.sort_unstable();
        v.get(((v.len() as f64 * p) as usize).min(v.len().saturating_sub(1))).copied().unwrap_or(0)
    };
    let pool_stats = |p: &[u32]| {
        let mut o: Vec<u64> = p.iter().map(|&i| deg[i as usize].0).collect();
        let mut i: Vec<u64> = p.iter().map(|&x| deg[x as usize].1).collect();
        json!({"n": p.len(), "follows_p50": pct(&mut o, 0.5), "follows_p99": pct(&mut o, 0.99),
               "followers_p50": pct(&mut i, 0.5), "followers_p99": pct(&mut i, 0.99)})
    };
    out.insert("pools".into(), json!({"popular": pool_stats(&popular), "ordinary": pool_stats(&ordinary)}));

    let mut plans: Vec<(String, AmongPlan)> =
        vec![("auto".into(), AmongPlan::Auto), ("scan".into(), AmongPlan::Scan), ("probe".into(), AmongPlan::Probe)];
    plans.extend(a.costs.iter().map(|&c| (format!("cost{c}"), AmongPlan::Cost(c))));
    let rels = [FOLLOW, BLOCK];
    let mut rng = Rng::new(a.seed);
    let mut shapes = Vec::new();
    for &n in &a.sizes {
        let k = (a.actors / n).clamp(10, a.sets.max(10));
        let hs = hubs(n);
        for shape in ["popular", "ordinary", "followers"] {
            let sets: Vec<Vec<u32>> = (0..k)
                .map(|_| match shape {
                    "popular" => pick(&mut rng, &popular, n),
                    "ordinary" => pick(&mut rng, &ordinary, n),
                    _ => {
                        let h = hs[rng.below(hs.len() as u64) as usize];
                        let f = g.read(|cx| g.in_set(cx, FOLLOW, h)).unwrap();
                        pick(&mut rng, &f, n)
                    }
                })
                .collect();
            let dids: Vec<Vec<String>> = sets.iter().map(|s| s.iter().filter_map(|&i| g.did_of(i)).collect()).collect();
            let mut row = BTreeMap::new();
            let mut edges = Vec::new();
            for (name, plan) in &plans {
                let mut lat = Vec::new();
                let mut probed = 0u64;
                let mut late = 0;
                for s in &sets {
                    let deadline = Some(Instant::now() + Duration::from_secs(30));
                    let run = || g.read(|cx| g.edges_among(cx, s, &rels, a.limit, *plan, deadline)).unwrap();
                    run();
                    let t = Instant::now();
                    let r = run();
                    lat.push(t.elapsed().as_nanos() as u64);
                    match r {
                        None => late += 1,
                        Some(r) => {
                            probed += r.probed;
                            if name == "scan" {
                                edges.push((r.counts[0], r.counts[1], r.truncated));
                            }
                        }
                    }
                }
                row.insert(
                    name.clone(),
                    json!({"lat": summarize(lat), "probed_mean": probed as f64 / k as f64, "late": late}),
                );
            }
            // the handler's whole work: DIDs to ids, the edges, ids and keys back to DIDs and URIs
            let mut full = Vec::new();
            for d in &dids {
                let go = || {
                    let mut ids: Vec<u32> = d.iter().filter_map(|x| g.id_of(x)).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    let r =
                        g.read(|cx| g.edges_among(cx, &ids, &rels, a.limit, AmongPlan::Auto, None)).unwrap().unwrap();
                    r.edges
                        .iter()
                        .map(|&(rel, x, y, k)| (g.did_of(x), g.did_of(y), g.uri(rel, x, k)))
                        .collect::<Vec<_>>()
                        .len()
                };
                go();
                let t = Instant::now();
                std::hint::black_box(go());
                full.push(t.elapsed().as_nanos() as u64);
            }
            row.insert("auto_with_uris".into(), json!({"lat": summarize(full)}));
            let mut fol: Vec<u64> = edges.iter().map(|e| e.0).collect();
            let mut blk: Vec<u64> = edges.iter().map(|e| e.1).collect();
            let size: f64 = sets.iter().map(|s| s.len() as f64).sum::<f64>() / k as f64;
            let follows_mean = fol.iter().sum::<u64>() as f64 / k as f64;
            let mut sum_out: Vec<u64> = sets.iter().map(|s| s.iter().map(|&i| deg[i as usize].0).sum()).collect();
            eprintln!("{shape} n={n} sets={k} follows_mean={follows_mean:.1}");
            shapes.push(json!({
                "shape": shape, "n": n, "sets": k, "members_mean": size,
                "follows_p50": pct(&mut fol, 0.5), "follows_max": pct(&mut fol, 1.0), "follows_mean": follows_mean,
                "density_mean": follows_mean / (size * (size - 1.0)).max(1.0),
                "blocks_p50": pct(&mut blk, 0.5),
                "truncated": edges.iter().filter(|e| e.2).count(),
                "members_follows_sum_p50": pct(&mut sum_out, 0.5),
                "plans": row,
            }));
        }
    }
    out.insert("shapes".into(), json!(shapes));
    Ok(serde_json::to_value(out)?)
}
