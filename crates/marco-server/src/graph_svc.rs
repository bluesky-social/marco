//! marco.v1.Graph over the served graph.

use std::sync::Arc;

use connectrpc::{ConnectError, InboundStream, RequestContext, Response, ServiceRequest, ServiceResult};
use futures::StreamExt;
use marco_core::graph::VIA_MAX_LISTS;
use marco_core::read::{Hold, ReadCx, StorageError, R};
use marco_core::relation::parse_uri;
use marco_core::{Dir, Graph, ListRef, RelId, SetOp, ViaLists, ViaQuery, ViaTruncated, COUNT_EXACT};
use marco_proto::connect::marco::v1 as svc;
use marco_proto::v1::__buffa::oneof::{write_op, write_result};
use marco_proto::v1::*;

use crate::app::{App, Op, Outcome, MAX_BATCH};
use crate::pool::PoolError;

/// The Graph service; `gate` holds reads until marco is ready (the API
/// listener; the console's copy on the admin listener answers regardless).
pub struct GraphApi(pub Arc<App>, pub bool);

pub fn bad(m: impl Into<String>) -> ConnectError {
    ConnectError::invalid_argument(m)
}

pub fn rel_id(g: &Graph, name: &str) -> Result<RelId, ConnectError> {
    if name.is_empty() {
        return Err(bad("rel is required"));
    }
    g.relations().by_name(name).ok_or_else(|| bad(format!("no relation {name:?}")))
}

/// The named relations; none names every one a caller configured.
pub fn rel_ids(g: &Graph, names: &[String]) -> Result<Vec<RelId>, ConnectError> {
    if names.is_empty() {
        return Ok(g.relations().user());
    }
    names.iter().map(|n| rel_id(g, n)).collect()
}

pub fn dir_of(d: &buffa::EnumValue<marco_proto::v1::Dir>) -> Result<Dir, ConnectError> {
    match d.as_known() {
        Some(marco_proto::v1::Dir::DIR_OUT) => Ok(Dir::Out),
        Some(marco_proto::v1::Dir::DIR_IN) => Ok(Dir::In),
        _ => Err(bad("dir must be DIR_OUT or DIR_IN")),
    }
}

pub fn dir_pb(d: Dir) -> buffa::EnumValue<marco_proto::v1::Dir> {
    match d {
        Dir::Out => marco_proto::v1::Dir::DIR_OUT.into(),
        Dir::In => marco_proto::v1::Dir::DIR_IN.into(),
    }
}

/// Pairs a BlockPairs call takes: a thread's hydration is under 100, so
/// this only stops a runaway caller.
pub const MAX_BLOCK_PAIRS: usize = 1000;

/// Vias a Relationships call takes: a profile's viewer state needs three.
pub const MAX_VIAS: usize = 10;

/// A request's via, its names resolved and checked. The caller's lists stay
/// URIs until a read resolves them against the graph it reads.
#[derive(Clone, Debug)]
pub struct ViaArgs {
    from: ViaFrom,
    members: RelId,
    kind: Option<RelId>,
    dir: Dir,
}

#[derive(Clone, Debug)]
enum ViaFrom {
    Rel(RelId),
    Lists(Vec<String>),
}

impl ViaArgs {
    /// Via `i` of a Relationships request, or a BlockPairs request's
    /// (`pairs`), which takes neither `dir` nor `lists`.
    pub fn of(g: &Graph, v: &Via, i: usize, pairs: bool) -> Result<ViaArgs, ConnectError> {
        let e = |m: String| bad(format!("via {i}: {m}"));
        let rels = g.relations();
        let rel = |n: &str| rels.by_name(n).ok_or_else(|| e(format!("no relation {n:?}")));
        if v.members.is_empty() {
            return Err(e("members is required".into()));
        }
        let members = rel(&v.members)?;
        if !rels.is_list(members) {
            return Err(e(format!("members {:?} isn't a list relation", v.members)));
        }
        let kind = match v.kind.as_str() {
            "" => None,
            k => {
                let r = rel(k)?;
                if !rels.is_uri_subject(r) {
                    return Err(e(format!("kind {k:?}'s subjects aren't records")));
                }
                Some(r)
            }
        };
        let dir = match (pairs, v.dir.as_known()) {
            (true, Some(marco_proto::v1::Dir::DIR_UNSPECIFIED)) => Dir::Out,
            (true, _) => return Err(e("BlockPairs checks both ways: no dir".into())),
            (false, Some(marco_proto::v1::Dir::DIR_OUT)) => Dir::Out,
            (false, Some(marco_proto::v1::Dir::DIR_IN)) => Dir::In,
            (false, _) => return Err(e("dir must be DIR_OUT or DIR_IN".into())),
        };
        let from = match (v.rel.as_str(), v.lists.is_empty()) {
            ("", true) => return Err(e("rel or lists is required".into())),
            (_, false) if !v.rel.is_empty() => return Err(e("rel or lists, not both".into())),
            (r, true) => {
                let r = rel(r)?;
                if rels.get(r).subject_collection.is_none() {
                    return Err(e(format!("{:?} has no subject_collection: its subjects aren't lists", v.rel)));
                }
                ViaFrom::Rel(r)
            }
            (_, false) => {
                if pairs {
                    return Err(e("BlockPairs reads both sides' lists: no lists".into()));
                }
                if dir != Dir::Out {
                    return Err(e("lists are the actor's own: DIR_OUT only".into()));
                }
                if v.lists.len() > VIA_MAX_LISTS {
                    return Err(e(format!("at most {VIA_MAX_LISTS} lists")));
                }
                if let Some(l) = v.lists.iter().find(|l| parse_uri(l).is_none()) {
                    return Err(e(format!("{l:?} isn't a record URI")));
                }
                ViaFrom::Lists(v.lists.clone())
            }
        };
        Ok(ViaArgs { from, members, kind, dir })
    }

    /// The via as the graph reads it: a caller's lists marco doesn't know
    /// hold nobody.
    pub fn query(&self, g: &Graph) -> ViaQuery {
        let lists = match &self.from {
            ViaFrom::Rel(r) => ViaLists::Rel(*r),
            ViaFrom::Lists(l) => {
                let mut ids: Vec<u32> = l.iter().filter_map(|u| g.id_of(u)).collect();
                ids.sort_unstable();
                ids.dedup();
                ViaLists::Given(ids)
            }
        };
        ViaQuery { lists, members: self.members, kind: self.kind, dir: self.dir }
    }
}

/// A Relationships request's vias.
pub fn vias_of(g: &Graph, vias: &[Via]) -> Result<Vec<ViaArgs>, ConnectError> {
    if vias.len() > MAX_VIAS {
        return Err(bad(format!("at most {MAX_VIAS} vias")));
    }
    vias.iter().enumerate().map(|(i, v)| ViaArgs::of(g, v, i, false)).collect()
}

/// A list's URI, or empty.
fn list_uri(g: &Graph, l: Option<u32>) -> String {
    l.and_then(|l| g.did_of(l)).unwrap_or_default()
}

fn limit(l: u32, def: u32, max: u32) -> usize {
    (if l == 0 { def } else { l.min(max) }) as usize
}

fn ops_of(ops: &[WriteOp]) -> Result<Vec<Op>, ConnectError> {
    if ops.len() > MAX_BATCH {
        return Err(bad(format!("at most {MAX_BATCH} ops a message")));
    }
    ops.iter()
        .enumerate()
        .map(|(i, o)| match &o.op {
            Some(write_op::Op::Add(a)) => Ok(Op::Add {
                uri: a.uri.clone(),
                subject: a.subject.clone(),
                list: (!a.list.is_empty()).then(|| a.list.clone()),
                time_us: a.time_us,
            }),
            Some(write_op::Op::Delete(d)) => Ok(Op::Delete { uri: d.uri.clone(), time_us: d.time_us }),
            None => Err(bad(format!("op {i} is neither add nor delete"))),
        })
        .collect()
}

fn result_pb(o: Outcome) -> WriteResult {
    let outcome = match o {
        Outcome::Added => write_result::Outcome::Added(true),
        Outcome::Exists => write_result::Outcome::Exists(true),
        Outcome::Deleted => write_result::Outcome::Deleted(true),
        Outcome::Removed(s) => write_result::Outcome::Removed(s),
        Outcome::NotFound => write_result::Outcome::NotFound(true),
        Outcome::Error(e) => write_result::Outcome::Error(e),
    };
    WriteResult { outcome: Some(outcome), ..Default::default() }
}

impl GraphApi {
    fn writable(&self) -> Result<(), ConnectError> {
        if self.0.cfg.read_only {
            return Err(ConnectError::failed_precondition("this marco is --read-only"));
        }
        Ok(())
    }

    async fn write_ops(&self, ops: &[WriteOp], cursor: &str) -> Result<Vec<WriteResult>, ConnectError> {
        self.writable()?;
        let ops = ops_of(ops)?;
        App::validate(&self.0.graph(), &ops).map_err(bad)?;
        let out = self.0.apply(&ops, cursor).await.map_err(|e| ConnectError::unavailable(format!("{e:#}")))?;
        Ok(out.into_iter().map(result_pb).collect())
    }

    fn readable(&self) -> Result<(), ConnectError> {
        if self.1 {
            self.0.read_gate().map_err(ConnectError::unavailable)?;
        }
        Ok(())
    }
}

/// A read's error as an RPC's: a page that failed its sum is data loss, a
/// request too big for the cache is resource_exhausted, and one the disk
/// couldn't give is unavailable.
pub fn storage_error(e: StorageError) -> ConnectError {
    match e {
        StorageError::Checksum(_) => ConnectError::data_loss(e.to_string()),
        StorageError::TooBig { .. } => ConnectError::resource_exhausted(e.to_string()),
        _ => ConnectError::unavailable(e.to_string()),
    }
}

/// How long a request that missed waits for a miss pool thread, at most.
pub const BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Runs a request's read, response building included, as passes of `f`
/// over the published copy of the graph. The first pass runs on this
/// worker and never waits. A request that misses moves whole to the miss
/// pool, where its passes fetch what they missed in between; each pass
/// loads the published copy afresh, so no request holds a copy while it
/// waits on disk and the writer's swap never waits on one. `rpc` labels
/// the request's pages and rounds histograms. A pass may need at most
/// [`marco_core::read::Engine::page_limit`] pages at once.
pub async fn run<T: Send + 'static>(
    app: &Arc<App>,
    ctx: &RequestContext,
    rpc: &'static str,
    f: impl Fn(&Graph, &ReadCx<'_>) -> R<T> + Send + 'static,
) -> Result<T, ConnectError> {
    run_with(app, ctx, rpc, false, f).await
}

/// [`run`] for the console's reads, which scan whole lists: a pass may need
/// half the cache at once, as a blocking job's may.
pub async fn run_admin<T: Send + 'static>(
    app: &Arc<App>,
    ctx: &RequestContext,
    rpc: &'static str,
    f: impl Fn(&Graph, &ReadCx<'_>) -> R<T> + Send + 'static,
) -> Result<T, ConnectError> {
    run_with(app, ctx, rpc, true, f).await
}

async fn run_with<T: Send + 'static>(
    app: &Arc<App>,
    ctx: &RequestContext,
    rpc: &'static str,
    job: bool,
    f: impl Fn(&Graph, &ReadCx<'_>) -> R<T> + Send + 'static,
) -> Result<T, ConnectError> {
    let first = {
        let g = app.graph();
        app.engine.pass(&mut Hold::none(), |cx| f(&g, cx))
    };
    let m = match first {
        Ok(t) => {
            observe(rpc, 0, 0);
            return Ok(t);
        }
        Err(m) => m,
    };
    crate::metrics::set_path("pool");
    let limit = if job { app.engine.job_limit() } else { app.engine.page_limit() };
    if m.needed() > limit {
        return Err(storage_error(StorageError::TooBig { pages: m.needed(), limit }));
    }
    let wait = ctx.time_remaining().map_or(BUSY_WAIT, |r| r.min(BUSY_WAIT));
    let a = app.clone();
    let r = app
        .miss_pool
        .run(wait, move || {
            let mut hold = if job { Hold::job(&a.engine) } else { Hold::request(&a.engine) };
            let r = a.engine.resume_blocking(&mut hold, Some(m), |cx| {
                let g = a.graph();
                f(&g, cx)
            });
            (r, hold.rounds(), hold.fetched())
        })
        .await;
    match r {
        Err(PoolError::Busy) => Err(ConnectError::resource_exhausted("storage busy")),
        Err(PoolError::Panicked) => Err(ConnectError::internal("the read panicked")),
        Ok((r, rounds, pages)) => {
            observe(rpc, rounds, pages);
            r.map_err(storage_error)
        }
    }
}

fn observe(rpc: &str, rounds: u32, pages: u64) {
    crate::metrics::REQUEST_ROUNDS.with_label_values(&[rpc]).observe(rounds as f64);
    crate::metrics::REQUEST_PAGES.with_label_values(&[rpc]).observe(pages as f64);
}

/// Has, its relation resolved: what the handler and the tests run.
pub fn has_(g: &Graph, cx: &ReadCx<'_>, rel: RelId, req: &HasRequest) -> R<HasResponse> {
    let hidden = g.hidden(&req.from) || g.hidden(&req.to);
    let uri = if hidden { None } else { g.has(cx, rel, &req.from, &req.to)? };
    Ok(HasResponse { uri: uri.unwrap_or_default(), ..Default::default() })
}

/// Relationships, its names resolved, and the via reads its caps cut.
pub fn relationships_(
    g: &Graph,
    cx: &ReadCx<'_>,
    rels: &[RelId],
    vias: &[ViaArgs],
    req: &RelationshipsRequest,
) -> R<(RelationshipsResponse, ViaTruncated)> {
    let others: Vec<&str> = req.others.iter().map(String::as_str).collect();
    let rows = g.relationships(cx, &req.actor, &others, rels);
    let actor_hidden = g.hidden(&req.actor);
    // a hidden actor or other answers no via, as it has no edges
    let via = if vias.is_empty() {
        Ok((Vec::new(), ViaTruncated::default()))
    } else {
        let actor = g.id_of(&req.actor).filter(|_| !actor_hidden);
        let ids: Vec<Option<u32>> = others.iter().map(|o| g.id_of(o).filter(|_| !g.hidden(o))).collect();
        let qs: Vec<ViaQuery> = vias.iter().map(|v| v.query(g)).collect();
        g.relationships_via(cx, actor, &ids, &qs)
    };
    // both asked before either's miss returns, so one pass names both's pages
    let (rows, (via, truncated)) = (rows?, via?);
    let relationships = req
        .others
        .iter()
        .zip(rows)
        .enumerate()
        .map(|(j, (o, per))| {
            let hide = actor_hidden || g.hidden(o);
            Relationship {
                other: o.clone(),
                rels: rels
                    .iter()
                    .zip(per)
                    .map(|(&r, x)| RelationshipEdge {
                        rel: g.relations().get(r).name.clone(),
                        out_uri: if hide { String::new() } else { x.out.unwrap_or_default() },
                        in_uri: if hide { String::new() } else { x.inn.unwrap_or_default() },
                        ..Default::default()
                    })
                    .collect(),
                via_lists: via.get(j).map_or_else(Vec::new, |v| v.iter().map(|&l| list_uri(g, l)).collect()),
                ..Default::default()
            }
        })
        .collect();
    Ok((RelationshipsResponse { relationships, ..Default::default() }, truncated))
}

/// BlockPairs, its names resolved, and the via reads its caps cut. Without
/// a via the answer is the direct blocks alone, as before vias.
pub fn block_pairs_(
    g: &Graph,
    cx: &ReadCx<'_>,
    rels: &[RelId],
    via: Option<&ViaArgs>,
    req: &BlockPairsRequest,
) -> R<(BlockPairsResponse, ViaTruncated)> {
    let pairs: Vec<(&str, &str)> = req.pairs.iter().map(|p| (p.a.as_str(), p.b.as_str())).collect();
    let res = g.block_pairs(cx, &pairs, rels);
    let hidden = |(a, b): (&str, &str)| g.any_status() && (g.hidden(a) || g.hidden(b));
    let Some(via) = via else {
        let mut res = res?;
        for (r, &p) in res.iter_mut().zip(&pairs) {
            if (r.0 || r.1) && hidden(p) {
                *r = (false, false);
            }
        }
        let resp = BlockPairsResponse {
            a_blocks_b: res.iter().map(|r| r.0).collect(),
            b_blocks_a: res.iter().map(|r| r.1).collect(),
            blocked: res.iter().map(|r| r.0 || r.1).collect(),
            ..Default::default()
        };
        return Ok((resp, ViaTruncated::default()));
    };
    // an unknown or hidden DID blocks nobody and is blocked by nobody
    let ids: Vec<Option<(u32, u32)>> =
        pairs.iter().map(|&p| g.id_of(p.0).zip(g.id_of(p.1)).filter(|_| !hidden(p))).collect();
    let lists = g.block_pairs_via(cx, &ids, &via.query(g));
    let (res, (lists, truncated)) = (res?, lists?);
    let mut pend = Ok(());
    let mut uris = vec![(None, None); pairs.len()];
    for ((u, &(x, y)), id) in uris.iter_mut().zip(&res).zip(&ids) {
        let Some((a, b)) = *id else { continue };
        // a URI costs a lookup only on a hit
        if x {
            u.0 = item(&mut pend, g.block_uri(cx, rels, a, b));
        }
        if y {
            u.1 = item(&mut pend, g.block_uri(cx, rels, b, a));
        }
    }
    pend?;
    let mut resp = BlockPairsResponse::default();
    for (((&(x, y), u), l), id) in res.iter().zip(&uris).zip(&lists).zip(&ids) {
        let (x, y) = if id.is_some() { (x, y) } else { (false, false) };
        let (lx, ly) = (list_uri(g, l.0), list_uri(g, l.1));
        resp.blocked.push(x || y || !lx.is_empty() || !ly.is_empty());
        resp.a_blocks_b.push(x);
        resp.b_blocks_a.push(y);
        resp.a_blocks_b_uri.push(u.0.clone().unwrap_or_default());
        resp.b_blocks_a_uri.push(u.1.clone().unwrap_or_default());
        resp.a_blocks_b_list.push(lx);
        resp.b_blocks_a_list.push(ly);
    }
    Ok((resp, truncated))
}

pub fn list_out_(g: &Graph, cx: &ReadCx<'_>, rel: RelId, req: &ListOutRequest) -> R<ListOutResponse> {
    if g.hidden(&req.actor) {
        return Ok(ListOutResponse::default());
    }
    let cursor = (!req.cursor.is_empty()).then_some(req.cursor.as_str());
    let lim = limit(req.limit, 50, 1000);
    let (items, next) = g.out_list_opts(cx, rel, &req.actor, lim, cursor, req.distinct)?;
    let edges = items
        .into_iter()
        .filter(|(_, s)| !g.hidden(s))
        .map(|(uri, subject)| OutEdge { uri, subject, ..Default::default() })
        .collect();
    Ok(ListOutResponse { edges, cursor: next.unwrap_or_default(), ..Default::default() })
}

pub fn list_in_(g: &Graph, cx: &ReadCx<'_>, rel: RelId, req: &ListInRequest) -> R<ListInResponse> {
    if g.hidden(&req.subject) {
        return Ok(ListInResponse::default());
    }
    let cursor = (!req.cursor.is_empty()).then_some(req.cursor.as_str());
    let lim = limit(req.limit, 50, 1000);
    let (actors, next) = g.in_list(cx, rel, &req.subject, lim, cursor)?;
    let actors = actors.into_iter().filter(|a| !g.hidden(a)).collect();
    Ok(ListInResponse { actors, cursor: next.unwrap_or_default(), ..Default::default() })
}

/// `r`'s value, or the default with its Pending kept in `pend`: a handler
/// that reads one list per item carries on past a miss, so one pass names
/// every page the batch misses.
pub fn item<T: Default>(pend: &mut R<()>, r: R<T>) -> T {
    r.unwrap_or_else(|p| {
        *pend = Err(p);
        T::default()
    })
}

pub fn counts_(g: &Graph, cx: &ReadCx<'_>, rels: &[RelId], req: &CountsRequest) -> R<CountsResponse> {
    let mut pend = Ok(());
    let mut counts = Vec::with_capacity(req.actors.len());
    for a in &req.actors {
        let hide = g.hidden(a);
        let mut per = Vec::with_capacity(rels.len());
        for &r in rels {
            let (o, i) = if hide { (0, 0) } else { item(&mut pend, g.counts(cx, r, a)) };
            let subjects = match g.id_of(a) {
                Some(id) if o > 0 => item(&mut pend, g.out_distinct(cx, r, id)),
                _ => 0,
            };
            per.push(RelCounts {
                rel: g.relations().get(r).name.clone(),
                out_records: o,
                in_actors: i,
                out_subjects: subjects,
                ..Default::default()
            });
        }
        counts.push(ActorCounts { actor: a.clone(), rels: per, ..Default::default() });
    }
    pend?;
    Ok(CountsResponse { counts, ..Default::default() })
}

/// An Intersect request with its names resolved, but not its actors.
#[derive(Clone)]
pub struct IntersectArgs {
    pub a: (RelId, Dir, String),
    pub b: (RelId, Dir, String),
    pub op: SetOp,
    pub cursor: String,
    pub lim: usize,
    pub cap: usize,
}

impl IntersectArgs {
    pub fn of(g: &Graph, req: &IntersectRequest) -> Result<IntersectArgs, ConnectError> {
        let side = |s: &ListSide| -> Result<(RelId, Dir, String), ConnectError> {
            Ok((rel_id(g, &s.rel)?, dir_of(&s.dir)?, s.actor.clone()))
        };
        let op = match req.op.as_known() {
            Some(marco_proto::v1::SetOp::SET_OP_AND_NOT) => SetOp::AndNot,
            Some(_) => SetOp::And,
            None => return Err(bad("op must be SET_OP_AND or SET_OP_AND_NOT")),
        };
        // exact unless the caller caps it: the work is one pass over the side the page decodes anyway
        let cap = match (req.count, req.count_limit) {
            (false, _) => 0,
            (true, 0) => COUNT_EXACT,
            (true, l) => limit(l, 0, 10_000),
        };
        Ok(IntersectArgs {
            a: side(&req.a)?,
            b: side(&req.b)?,
            op,
            cursor: req.cursor.clone(),
            lim: limit(req.limit, 100, 10_000),
            cap,
        })
    }
}

/// Intersect; Err(None) is a cursor that isn't in the graph.
pub fn intersect_(g: &Graph, cx: &ReadCx<'_>, q: &IntersectArgs) -> R<Result<IntersectResponse, String>> {
    let side = |(rel, dir, actor): &(RelId, Dir, String)| -> Option<ListRef> {
        if g.hidden(actor) {
            return None;
        }
        g.id_of(actor).map(|actor| ListRef { rel: *rel, dir: *dir, actor })
    };
    let (a, b) = (side(&q.a), side(&q.b));
    let after = match q.cursor.as_str() {
        "" => None,
        c => match g.id_of(c) {
            Some(id) => Some(id),
            None => return Ok(Err("cursor isn't in this graph".into())),
        },
    };
    let (lim, cap, op) = (q.lim, q.cap, q.op);
    let len = |s: Option<ListRef>| match s {
        None => Ok(0),
        Some(s) => match s.dir {
            Dir::Out => g.out_count(cx, s.rel, s.actor),
            Dir::In => g.in_count(cx, s.rel, s.actor),
        },
    };
    let (ids, total) = match (a, b, op) {
        (Some(x), Some(y), _) => g.combine_counted(cx, x, y, op, lim, after, cap)?,
        // a without a missing (or hidden) b is all of a
        (Some(x), None, SetOp::AndNot) => {
            let mut v = match x.dir {
                Dir::Out => g.out_set(cx, x.rel, x.actor)?,
                Dir::In => g.in_set(cx, x.rel, x.actor)?,
            };
            let total = (cap > 0).then(|| (v.len().min(cap) as u64, v.len() > cap));
            v.retain(|&i| after.is_none_or(|a| i > a));
            v.truncate(lim);
            (v, total)
        }
        _ => (Vec::new(), (cap > 0).then_some((0, false))),
    };
    let (total, total_capped) = total.unwrap_or_default();
    let cursor = if ids.len() == lim { ids.last().and_then(|&i| g.did_of(i)) } else { None };
    let actors = ids.into_iter().filter_map(|id| g.did_of(id)).filter(|d| !g.hidden(d)).collect();
    Ok(Ok(IntersectResponse {
        actors,
        a_len: len(a)?,
        b_len: len(b)?,
        cursor: cursor.unwrap_or_default(),
        total,
        total_capped,
        ..Default::default()
    }))
}

/// A Filter request's drops and sets, their names resolved.
#[derive(Clone)]
pub struct FilterArgs {
    pub drops: Vec<(RelId, Dir)>,
    /// (rel, dir, the list's actor)
    pub sets: Vec<(RelId, Dir, String)>,
}

impl FilterArgs {
    pub fn of(g: &Graph, req: &FilterRequest) -> Result<FilterArgs, ConnectError> {
        let mut drops = Vec::new();
        let mut sets = Vec::new();
        for d in &req.drop {
            match d.member_of.as_option() {
                Some(m) => sets.push((rel_id(g, &m.rel)?, dir_of(&m.dir)?, m.actor.clone())),
                None => drops.push((rel_id(g, &d.rel)?, dir_of(&d.dir)?)),
            }
        }
        Ok(FilterArgs { drops, sets })
    }
}

pub fn filter_(g: &Graph, cx: &ReadCx<'_>, q: &FilterArgs, req: &FilterRequest) -> R<FilterResponse> {
    let drops = &q.drops;
    // an unknown or hidden list drops nobody
    let sets: Vec<ListRef> = q
        .sets
        .iter()
        .filter_map(|(rel, dir, a)| {
            g.id_of(a).filter(|_| !g.hidden(a)).map(|actor| ListRef { rel: *rel, dir: *dir, actor })
        })
        .collect();
    let viewer = g.id_of(&req.viewer);
    let ids: Vec<Option<u32>> = req.actors.iter().map(|a| g.id_of(a)).collect();
    let known: Vec<u32> = ids.iter().flatten().copied().collect();
    let kept: std::collections::HashSet<u32> = match viewer {
        Some(v) => g.filter_sets(cx, v, &known, drops, &sets)?.into_iter().collect(),
        None => g.filter_sets(cx, u32::MAX, &known, &[], &sets)?.into_iter().collect(),
    };
    let mut actors = Vec::new();
    let mut dropped = Vec::new();
    let mut pend = Ok(());
    for (a, id) in req.actors.iter().zip(&ids) {
        if g.hidden(a) {
            dropped.push(Dropped { actor: a.clone(), rel: "status".into(), ..Default::default() });
            continue;
        }
        match id {
            Some(x) if !kept.contains(x) => {
                // every reason's lookup, so one pass names every dropped actor's pages
                let mut why = Vec::new();
                if let Some(v) = viewer {
                    for &(r, d) in drops {
                        let yes = match d {
                            Dir::Out => item(&mut pend, g.has_id(cx, r, v, *x)).is_some(),
                            Dir::In => item(&mut pend, g.in_contains(cx, r, v, *x)),
                        };
                        why.push((yes, (r, d)));
                    }
                }
                for s in &sets {
                    why.push((item(&mut pend, g.side_contains(cx, *s, *x)), (s.rel, s.dir)));
                }
                let hit = why.into_iter().find(|w| w.0).map(|w| w.1);
                let (r, d) = hit.unwrap_or((0, Dir::Out));
                dropped.push(Dropped {
                    actor: a.clone(),
                    rel: g.relations().get(r).name.clone(),
                    dir: dir_pb(d),
                    ..Default::default()
                });
            }
            _ => actors.push(a.clone()),
        }
    }
    pend?;
    Ok(FilterResponse { actors, dropped, ..Default::default() })
}

/// The relations a BlockPairs request names: block by default, and no
/// list relation or relation whose subjects are records.
pub fn block_pair_rels(g: &Graph, names: &[String]) -> Result<Vec<RelId>, ConnectError> {
    let rels = if names.is_empty() {
        vec![g.relations().by_name("block").ok_or_else(|| bad("no relation \"block\": name rels"))?]
    } else {
        names.iter().map(|n| rel_id(g, n)).collect::<Result<Vec<_>, _>>()?
    };
    if let Some(&r) = rels.iter().find(|&&r| g.relations().is_list(r)) {
        return Err(bad(format!("{:?} is a list relation", g.relations().get(r).name)));
    }
    if let Some(&r) = rels.iter().find(|&&r| g.relations().is_uri_subject(r)) {
        return Err(bad(format!("{:?}'s subjects are records", g.relations().get(r).name)));
    }
    Ok(rels)
}

#[allow(refining_impl_trait)]
impl svc::Graph for GraphApi {
    async fn has(&self, ctx: RequestContext, req: ServiceRequest<'_, HasRequest>) -> ServiceResult<HasResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        let rel = rel_id(&self.0.graph(), &req.rel)?;
        Response::ok(run(&self.0, &ctx, "Graph/Has", move |g, cx| has_(g, cx, rel, &req)).await?)
    }

    async fn relationships(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, RelationshipsRequest>,
    ) -> ServiceResult<RelationshipsResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        if req.others.len() > 100 {
            return Err(bad("at most 100 others"));
        }
        let (rels, vias) = {
            let g = self.0.graph();
            let rels = if req.rels.is_empty() { g.relations().actor_rels() } else { rel_ids(&g, &req.rels)? };
            (rels, vias_of(&g, &req.via)?)
        };
        let (resp, cut) =
            run(&self.0, &ctx, "Graph/Relationships", move |g, cx| relationships_(g, cx, &rels, &vias, &req)).await?;
        crate::metrics::via_truncated("Graph/Relationships", cut);
        Response::ok(resp)
    }

    async fn block_pairs(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, BlockPairsRequest>,
    ) -> ServiceResult<BlockPairsResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        if req.pairs.len() > MAX_BLOCK_PAIRS {
            return Err(bad(format!("at most {MAX_BLOCK_PAIRS} pairs")));
        }
        let (rels, via) = {
            let g = self.0.graph();
            let via = req.via.as_option().map(|v| ViaArgs::of(&g, v, 0, true)).transpose()?;
            (block_pair_rels(&g, &req.rels)?, via)
        };
        let (resp, cut) =
            run(&self.0, &ctx, "Graph/BlockPairs", move |g, cx| block_pairs_(g, cx, &rels, via.as_ref(), &req)).await?;
        crate::metrics::via_truncated("Graph/BlockPairs", cut);
        Response::ok(resp)
    }

    async fn list_out(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListOutRequest>,
    ) -> ServiceResult<ListOutResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        let rel = rel_id(&self.0.graph(), &req.rel)?;
        Response::ok(run(&self.0, &ctx, "Graph/ListOut", move |g, cx| list_out_(g, cx, rel, &req)).await?)
    }

    async fn list_in(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, ListInRequest>,
    ) -> ServiceResult<ListInResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        let rel = rel_id(&self.0.graph(), &req.rel)?;
        Response::ok(run(&self.0, &ctx, "Graph/ListIn", move |g, cx| list_in_(g, cx, rel, &req)).await?)
    }

    async fn counts(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, CountsRequest>,
    ) -> ServiceResult<CountsResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        if req.actors.len() > 100 {
            return Err(bad("at most 100 actors"));
        }
        let rels = rel_ids(&self.0.graph(), &req.rels)?;
        Response::ok(run(&self.0, &ctx, "Graph/Counts", move |g, cx| counts_(g, cx, &rels, &req)).await?)
    }

    async fn intersect(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, IntersectRequest>,
    ) -> ServiceResult<IntersectResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        let q = IntersectArgs::of(&self.0.graph(), &req)?;
        let resp = run(&self.0, &ctx, "Graph/Intersect", move |g, cx| intersect_(g, cx, &q)).await?;
        Response::ok(resp.map_err(bad)?)
    }

    async fn filter(
        &self,
        ctx: RequestContext,
        req: ServiceRequest<'_, FilterRequest>,
    ) -> ServiceResult<FilterResponse> {
        self.readable()?;
        let req = req.to_owned_message();
        if req.actors.len() > 1000 {
            return Err(bad("at most 1000 actors"));
        }
        let q = FilterArgs::of(&self.0.graph(), &req)?;
        Response::ok(run(&self.0, &ctx, "Graph/Filter", move |g, cx| filter_(g, cx, &q, &req)).await?)
    }

    async fn write(&self, _: RequestContext, mut reqs: InboundStream<WriteRequest>) -> ServiceResult<WriteResponse> {
        self.writable()?;
        let mut results = Vec::new();
        let mut cursor = String::new();
        while let Some(m) = reqs.next().await {
            let m = m?.to_owned_message();
            results.extend(self.write_ops(&m.ops, &m.cursor).await?);
            if !m.cursor.is_empty() {
                cursor = m.cursor;
            }
        }
        Response::ok(WriteResponse { results, cursor, ..Default::default() })
    }

    async fn write_batch(
        &self,
        _: RequestContext,
        req: ServiceRequest<'_, WriteBatchRequest>,
    ) -> ServiceResult<WriteBatchResponse> {
        let req = req.to_owned_message();
        let results = self.write_ops(&req.ops, &req.cursor).await?;
        Response::ok(WriteBatchResponse { results, cursor: req.cursor, ..Default::default() })
    }
}
