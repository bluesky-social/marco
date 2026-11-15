//! Two hops in one read: whether a list a subscriber is connected to holds a
//! member, and which.
//!
//! A subscriber's lists are the subjects of its records in a relation whose
//! subjects are records (`listblock`), or lists the caller names (a viewer's
//! mute lists). The member's side is its in set in the list relation, the
//! lists it's on, which is never decoded: the subscriber's lists, sorted,
//! probe it. So the subscriber's side is the one read whole, and that's what
//! the caps bound. A member on no list costs one count and skips the read.
//!
//! Storing "S blocks M through a list" as edges would fan out on every write
//! (a list's members times its subscribers), so the hops meet at read time on
//! the list's id, which a subscription's subject and a listitem's actor share.

use rustc_hash::FxHashMap;

use super::{item, Dir, Graph};
use crate::read::{ReadCx, R};
use crate::relation::RelId;

/// The request actor's newest records a via reads.
pub const VIA_ACTOR_CAP: usize = 1000;
/// Each other actor's newest records a via reads, and each side's of a pair.
pub const VIA_OTHER_CAP: usize = 100;
/// The most lists a caller names in one via.
pub const VIA_MAX_LISTS: usize = 1000;
/// Masked base records (deleted or replaced since the checkpoint) a
/// subscriber's read passes per record of its cap, before it stops and
/// counts as cut: 10,000 for the actor's, 1,000 for an other's. Unbounded,
/// a mass unsubscribe of 150,000 cost every request naming that account
/// ~5 ms until the next checkpoint (via-bench), and ~31 ns more per record.
pub const VIA_MASKED_PER_RECORD: usize = 10;

/// Where a subscriber's lists come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViaLists {
    /// The subjects of its records in this relation, the newest up to a cap.
    Rel(RelId),
    /// The caller's, as ids, sorted and deduplicated: the request actor's own.
    Given(Vec<u32>),
}

/// One direction of a two-hop check, its names resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViaQuery {
    pub lists: ViaLists,
    /// The list relation whose edges name the members (listitem).
    pub members: RelId,
    /// Only a list that some live actor's record in this relation names
    /// (`modlist`, whose one record of a list is its owner's).
    pub kind: Option<RelId>,
    /// Out: the actor's lists hold the other. In: the other's hold the actor.
    pub dir: Dir,
}

/// Subscriber reads cut at their cap: out for the request actor's (a pair's
/// a), in for each other's (a pair's b).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViaTruncated {
    pub out: u64,
    pub inn: u64,
}

/// Per pair: a list `a` subscribes to that holds `b`, and the reverse.
pub type PairLists = Vec<(Option<u32>, Option<u32>)>;

/// One call's reads, each done once: a subscriber's lists, whether a member
/// is on any list.
#[derive(Default)]
struct Memo {
    subs: FxHashMap<(RelId, u32, usize), Vec<u32>>,
    on: FxHashMap<(RelId, u32), bool>,
    truncated: ViaTruncated,
}

impl Graph {
    /// `s`'s lists in `rel`: the subjects of its newest `cap` live records,
    /// sorted and deduplicated, and whether it has more records than that
    /// (or more deletes since the checkpoint than the read passes, in which
    /// case they're the newest it found). The read costs at most `cap`
    /// records and `cap` × [`VIA_MASKED_PER_RECORD`] masked ones.
    pub fn via_subscriptions(&self, cx: &ReadCx<'_>, rel: RelId, s: u32, cap: usize) -> R<(Vec<u32>, bool)> {
        let masked = cap.saturating_mul(VIA_MASKED_PER_RECORD);
        let (page, stopped) = self.out_page_masked(cx, rel, s, cap.saturating_add(1), None, masked)?;
        let more = page.len() > cap || stopped;
        let mut ids: Vec<u32> = page.into_iter().take(cap).map(|(_, l)| l).collect();
        ids.sort_unstable();
        ids.dedup();
        Ok((ids, more))
    }

    /// The lowest-id list of sorted `lists` whose `members` edges include
    /// `m` and that answers ([`Graph::via_live`]).
    pub fn via_match(
        &self,
        cx: &ReadCx<'_>,
        members: RelId,
        kind: Option<RelId>,
        m: u32,
        lists: &[u32],
    ) -> R<Option<u32>> {
        if lists.is_empty() {
            return Ok(None);
        }
        let hits = self.in_intersect(cx, members, m, lists, usize::MAX)?;
        // a later hit can't answer while an earlier one's check is pending;
        // the rest are still asked, so one pass names every page they need
        let mut pend = Ok(());
        for l in hits {
            if item(&mut pend, self.via_live(cx, kind, l)) == Some(true) && pend.is_ok() {
                return Ok(Some(l));
            }
        }
        pend.map(|()| None)
    }

    /// Whether list `l` can answer: its owner isn't hidden, and with `kind`,
    /// an actor that isn't hidden has a record of `kind` naming it.
    pub fn via_live(&self, cx: &ReadCx<'_>, kind: Option<RelId>, l: u32) -> R<bool> {
        let statuses = self.any_status();
        if statuses && self.did_of(l).is_some_and(|u| self.hidden(&u)) {
            return Ok(false);
        }
        let Some(k) = kind else { return Ok(true) };
        if !statuses {
            return Ok(self.in_count(cx, k, l)? > 0);
        }
        // a $self relation's one actor is the owner; another relation's
        // first live actor is almost always its first
        const STEP: usize = 16;
        let mut after = None;
        loop {
            let page = self.in_page(cx, k, l, STEP, after)?;
            if page.iter().any(|&x| !self.hidden_id(x)) {
                return Ok(true);
            }
            if page.len() < STEP {
                return Ok(false);
            }
            after = page.last().copied();
        }
    }

    /// Whether `m` is on any list of `members`, asked once a call.
    fn via_on(&self, cx: &ReadCx<'_>, memo: &mut Memo, members: RelId, m: u32) -> R<bool> {
        if let Some(&on) = memo.on.get(&(members, m)) {
            return Ok(on);
        }
        let on = self.in_count(cx, members, m)? > 0;
        memo.on.insert((members, m), on);
        Ok(on)
    }

    /// `s`'s lists in `rel`, read once a call; a cut read counts toward `dir`.
    fn via_subs<'m>(
        &self,
        cx: &ReadCx<'_>,
        memo: &'m mut Memo,
        rel: RelId,
        s: u32,
        cap: usize,
        dir: Dir,
    ) -> R<&'m [u32]> {
        let key = (rel, s, cap);
        if !memo.subs.contains_key(&key) {
            let (ids, more) = self.via_subscriptions(cx, rel, s, cap)?;
            if more {
                match dir {
                    Dir::Out => memo.truncated.out += 1,
                    Dir::In => memo.truncated.inn += 1,
                }
            }
            memo.subs.insert(key, ids);
        }
        Ok(&memo.subs[&key])
    }

    /// Per other and via, in order: the list that answers, or None. An
    /// actor or other that's None (unknown, or hidden) answers None. Out
    /// reads the actor's lists once, at the first other on any list; In
    /// reads each other's lists, unless the actor is on no list at all.
    pub fn relationships_via(
        &self,
        cx: &ReadCx<'_>,
        actor: Option<u32>,
        others: &[Option<u32>],
        vias: &[ViaQuery],
    ) -> R<(Vec<Vec<Option<u32>>>, ViaTruncated)> {
        let mut out = vec![vec![None; vias.len()]; others.len()];
        let Some(a) = actor else { return Ok((out, ViaTruncated::default())) };
        let mut ids: Vec<u32> = others.iter().flatten().copied().collect();
        ids.sort_unstable();
        ids.dedup();
        let mut memo = Memo::default();
        let mut pend = Ok(());
        let mut hits = vec![None; ids.len()];
        for (vi, v) in vias.iter().enumerate() {
            hits.iter_mut().for_each(|h| *h = None);
            match (v.dir, &v.lists) {
                (Dir::Out, lists) => {
                    let mut none = matches!(lists, ViaLists::Given(l) if l.is_empty());
                    for (k, &o) in ids.iter().enumerate() {
                        if none || item(&mut pend, self.via_on(cx, &mut memo, v.members, o)) != Some(true) {
                            continue;
                        }
                        let lists = match lists {
                            ViaLists::Given(l) => &l[..],
                            ViaLists::Rel(r) => {
                                match item(&mut pend, self.via_subs(cx, &mut memo, *r, a, VIA_ACTOR_CAP, Dir::Out)) {
                                    Some(l) => l,
                                    None => continue,
                                }
                            }
                        };
                        if lists.is_empty() {
                            // no subscriptions: no other needs its count asked
                            none = true;
                            continue;
                        }
                        hits[k] = item(&mut pend, self.via_match(cx, v.members, v.kind, o, lists)).flatten();
                    }
                }
                // a caller's lists are the actor's own: refused for In before here
                (Dir::In, ViaLists::Given(_)) => continue,
                (Dir::In, ViaLists::Rel(r)) => {
                    if item(&mut pend, self.via_on(cx, &mut memo, v.members, a)) != Some(true) {
                        continue;
                    }
                    for (k, &o) in ids.iter().enumerate() {
                        // the actor named as an other is still the actor: its cap, its one read
                        let (cap, side) = if o == a { (VIA_ACTOR_CAP, Dir::Out) } else { (VIA_OTHER_CAP, Dir::In) };
                        let Some(lists) = item(&mut pend, self.via_subs(cx, &mut memo, *r, o, cap, side)) else {
                            continue;
                        };
                        hits[k] = item(&mut pend, self.via_match(cx, v.members, v.kind, a, lists)).flatten();
                    }
                }
            }
            for (j, o) in others.iter().enumerate() {
                if let Some(o) = o {
                    out[j][vi] = hits[ids.binary_search(o).unwrap()];
                }
            }
        }
        pend.map(|()| (out, memo.truncated))
    }

    /// Per pair, both ways: a list `a` subscribes to that holds `b`, and one
    /// `b` subscribes to that holds `a`, through `via`'s relation (its `dir`
    /// is ignored). A pair that's None (unknown, or hidden) answers neither.
    /// Each side's member count comes before its subscriber's read, and each
    /// subscriber is read once a call, its newest [`VIA_OTHER_CAP`] records.
    pub fn block_pairs_via(
        &self,
        cx: &ReadCx<'_>,
        pairs: &[Option<(u32, u32)>],
        via: &ViaQuery,
    ) -> R<(PairLists, ViaTruncated)> {
        let mut out = vec![(None, None); pairs.len()];
        let ViaLists::Rel(r) = via.lists else { return Ok((out, ViaTruncated::default())) };
        let mut memo = Memo::default();
        let mut pend = Ok(());
        // (subscriber, member, the side its read counts toward)
        let way = |s: u32, m: u32, dir: Dir, memo: &mut Memo, pend: &mut R<()>| -> Option<u32> {
            if item(pend, self.via_on(cx, memo, via.members, m)) != Some(true) {
                return None;
            }
            let lists = item(pend, self.via_subs(cx, memo, r, s, VIA_OTHER_CAP, dir))?;
            item(pend, self.via_match(cx, via.members, via.kind, m, lists)).flatten()
        };
        for (p, o) in pairs.iter().zip(&mut out) {
            if let &Some((a, b)) = p {
                *o = (way(a, b, Dir::Out, &mut memo, &mut pend), way(b, a, Dir::In, &mut memo, &mut pend));
            }
        }
        pend.map(|()| (out, memo.truncated))
    }
}
