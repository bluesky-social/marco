//! EdgesAmong: every edge of some relations whose actor and subject are
//! both in one set of actors.
//!
//! The work is finding each edge once. A member with a short out log scans
//! it, testing each subject against a hash of the members. A member whose
//! out log is long next to the set is probed instead: it joins `probe`, and
//! every member's in set is asked, block by block, which of `probe` it
//! holds. An edge a→s is then found from a's out log when a scans, and from
//! s's in set when a is probed, so never twice and never missed.

use std::time::Instant;

use rustc_hash::FxHashSet;

use super::{item, Graph};
use crate::obs::Op;
use crate::outlog::BLOCK;
use crate::read::{ReadCx, R};
use crate::relation::RelId;

/// How EdgesAmong finds each member's edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmongPlan {
    /// By out log length against the set's size ([`AMONG_PROBE_COST`]).
    Auto,
    /// Probe a member whose out log is longer than this many times the set.
    Cost(u64),
    /// Every member scans its out log.
    Scan,
    /// Every member is probed in the others' in sets.
    Probe,
}

/// What probing one member in one in set costs, in out records scanned:
/// [`AmongPlan::Auto`] probes a member whose out log is longer than this
/// many times the set.
pub const AMONG_PROBE_COST: u64 = 64;

/// One edge: (relation, actor id, subject id, the newest record's key).
pub type AmongEdge = (RelId, u32, u32, u64);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Among {
    /// Up to the limit: by relation in the order asked, then actor and subject in id order.
    pub edges: Vec<AmongEdge>,
    /// Edges per relation, in the order asked, past the limit too.
    pub counts: Vec<u64>,
    /// There were more edges than the limit.
    pub truncated: bool,
    /// Members probed rather than scanned, summed over the relations.
    pub probed: u64,
}

impl Graph {
    /// The edges of `rels` among `members` (ids, sorted and unique): each
    /// (actor, subject) pair once, with its newest record. None when
    /// `deadline` passed first. A list relation's actors are lists, so the
    /// caller refuses those, and relations whose subjects are records.
    pub fn edges_among(
        &self,
        cx: &ReadCx<'_>,
        members: &[u32],
        rels: &[RelId],
        limit: usize,
        plan: AmongPlan,
        deadline: Option<Instant>,
    ) -> R<Option<Among>> {
        debug_assert!(members.windows(2).all(|w| w[0] < w[1]));
        let g = self.obs.op(Op::EdgesAmong, self.obs.multi(), members.len() * rels.len());
        let set: FxHashSet<u32> = members.iter().copied().collect();
        let mut out = Among { counts: vec![0; rels.len()], ..Default::default() };
        let mut pend = Ok(());
        let late = || deadline.is_some_and(|d| Instant::now() >= d);
        for (ri, &rel) in rels.iter().enumerate() {
            let probe: Vec<u32> = match plan {
                AmongPlan::Scan => Vec::new(),
                AmongPlan::Probe => members.to_vec(),
                AmongPlan::Auto | AmongPlan::Cost(_) => {
                    let c = if let AmongPlan::Cost(c) = plan { c } else { AMONG_PROBE_COST };
                    let over = c * members.len() as u64;
                    members
                        .iter()
                        .copied()
                        .filter(|&a| item(&mut pend, self.out_count(cx, rel, a)) > Some(over))
                        .collect()
                }
            };
            out.probed += probe.len() as u64;
            // (actor, subject) for every probed actor's edge
            let mut probed: Vec<(u32, u32)> = Vec::new();
            if !probe.is_empty() {
                for (i, &s) in members.iter().enumerate() {
                    if i % 64 == 0 && late() {
                        return Ok(None);
                    }
                    if let Some(hits) = item(&mut pend, self.in_intersect(cx, rel, s, &probe, usize::MAX)) {
                        probed.extend(hits.into_iter().map(|a| (a, s)));
                    }
                }
                probed.sort_unstable();
            }
            let mut hits: Vec<(u32, u64)> = Vec::new();
            let mut p = 0;
            for (i, &a) in members.iter().enumerate() {
                if i % 64 == 0 && late() {
                    return Ok(None);
                }
                let room = limit - out.edges.len();
                if probe.binary_search(&a).is_ok() {
                    let from = p;
                    while p < probed.len() && probed[p].0 == a {
                        p += 1;
                    }
                    let mine = &probed[from..p];
                    out.counts[ri] += mine.len() as u64;
                    for &(_, s) in mine.iter().take(room) {
                        let k = item(&mut pend, self.out_find_subject(cx, rel, a, s)).flatten().unwrap_or_default();
                        out.edges.push((rel, a, s, k));
                    }
                    out.truncated |= mine.len() > room;
                    continue;
                }
                hits.clear();
                if item(&mut pend, self.scan_among(cx, rel, a, &set, room, &mut hits)).is_none() {
                    continue;
                }
                out.counts[ri] += hits.len() as u64;
                out.truncated |= hits.len() > room;
                out.edges.extend(hits.iter().take(room).map(|&(s, k)| (rel, a, s, k)));
            }
        }
        pend?;
        self.obs.done(g, out.edges.len());
        Ok(Some(out))
    }

    /// `actor`'s subjects in `set`, sorted, each with its newest record's
    /// key; only the first `room` get their key (the rest are 0).
    fn scan_among(
        &self,
        cx: &ReadCx<'_>,
        rel: RelId,
        actor: u32,
        set: &FxHashSet<u32>,
        room: usize,
        hits: &mut Vec<(u32, u64)>,
    ) -> R<()> {
        if self.state[rel as usize].out.contains_key(&actor) {
            // a delta: the merged records, by rkey, so a subject's last is its newest
            for (k, s) in self.out_all(cx, rel, actor)? {
                if set.contains(&s) {
                    hits.push((s, k));
                }
            }
            hits.sort_by_key(|&(s, k)| (s, std::cmp::Reverse(k)));
            hits.dedup_by_key(|h| h.0);
            return Ok(());
        }
        let Some(l) = self.base_out(cx, rel, actor)? else { return Ok(()) };
        // (subject, position): positions are rkey order, so a subject's last is its newest
        let mut found: Vec<(u32, usize)> = Vec::new();
        let mut pend = Ok(());
        for b in 0..l.blocks() {
            let Some(subj) = item(&mut pend, l.block_subjects(cx, b)) else { continue };
            for (i, c) in subj.as_chunks::<4>().0.iter().enumerate() {
                let s = u32::from_le_bytes(*c);
                if set.contains(&s) {
                    found.push((s, b * BLOCK + i));
                }
            }
        }
        pend?;
        found.sort_unstable_by_key(|&(s, p)| (s, std::cmp::Reverse(p)));
        found.dedup_by_key(|f| f.0);
        let mut keys = [0u64; BLOCK];
        let mut at = usize::MAX;
        // the keys a page needs, block by block
        let mut want: Vec<(usize, usize)> = found.iter().take(room).enumerate().map(|(j, f)| (f.1, j)).collect();
        want.sort_unstable();
        hits.extend(found.iter().map(|f| (f.0, 0)));
        for (pos, j) in want {
            if pos / BLOCK != at {
                at = pos / BLOCK;
                l.block_keys(cx, at, &mut keys)?;
            }
            hits[j].1 = keys[pos % BLOCK];
        }
        Ok(())
    }
}
