//! Two hops in one read: whether a list a subscriber is connected to holds a
//! member, and which.
//!
//! A subscriber's lists are the subjects of its records in a relation whose
//! subjects are records (`listblock`), or lists the caller names (a viewer's
//! mute lists). The member's side is its in set in the list relation, the
//! lists it's on, which is never decoded: the subscriber's lists, sorted,
//! probe it. So the subscriber's side is the one read whole, and that's what
//! the cap bounds. A member on no list costs one count and skips the read.
//!
//! Every subscriber's read has the same cap, whichever call or side names
//! it, so two calls asking about the same pair agree. A read the cap (or a
//! run of uncheckpointed deletes) stops short says so with each empty answer
//! it gave.
//!
//! Storing "S blocks M through a list" as edges would fan out on every write
//! (a list's members times its subscribers), so the hops meet at read time on
//! the list's id, which a subscription's subject and a listitem's actor share.

use rustc_hash::FxHashMap;

use super::{item, Dir, Graph};
use crate::read::{ReadCx, R};
use crate::relation::RelId;

/// The most of a subscriber's newest records a via reads, unless the server
/// says otherwise (`marco serve --via-max-records`).
pub const VIA_MAX_RECORDS: usize = 1000;
/// The most lists a caller names in one via.
pub const VIA_MAX_LISTS: usize = 1000;
/// Base records a subscriber's read passes, per record it may read, that
/// writes since the last checkpoint deleted or replaced. Past that the read
/// stops with what it found and counts as cut, so an account that just
/// dropped many subscriptions costs a bounded read until a checkpoint folds
/// the deletes.
pub const VIA_MASKED_PER_RECORD: usize = 10;

/// Where a subscriber's lists come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViaLists {
    /// The subjects of its records in this relation, the newest up to the cap.
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

/// Both directions of every pair, from one relation's lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairQuery {
    pub rel: RelId,
    pub members: RelId,
    pub kind: Option<RelId>,
}

/// Why a subscriber's read stopped before its oldest record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViaCut {
    /// It didn't: every record was read.
    #[default]
    None,
    /// The subscriber has more records than the cap.
    Cap,
    /// The read passed as many deleted or replaced records as it may
    /// ([`VIA_MASKED_PER_RECORD`]).
    Deletes,
}

/// One answer: the list that matched, or whether a cut read may have
/// missed one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViaHit {
    pub list: Option<u32>,
    /// No list matched, and the subscriber's read stopped short.
    pub truncated: bool,
}

/// A call's subscriber reads that stopped short, by why. Each subscriber
/// is read, and counted, once a call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViaCuts {
    pub cap: u64,
    pub deletes: u64,
}

/// One call's reads, each done once: a subscriber's lists, whether a member
/// is on any list.
#[derive(Default)]
struct Memo {
    subs: FxHashMap<(RelId, u32), (Vec<u32>, ViaCut)>,
    on: FxHashMap<(RelId, u32), bool>,
    cuts: ViaCuts,
}

impl Graph {
    /// `s`'s lists in `rel`: the subjects of its newest `cap` live records,
    /// sorted and deduplicated, and whether the read stopped before its
    /// oldest record. The read costs at most `cap` records and `cap` ×
    /// [`VIA_MASKED_PER_RECORD`] masked ones.
    pub fn via_subscriptions(&self, cx: &ReadCx<'_>, rel: RelId, s: u32, cap: usize) -> R<(Vec<u32>, ViaCut)> {
        let masked = cap.saturating_mul(VIA_MASKED_PER_RECORD);
        let (page, stopped) = self.out_page_masked(cx, rel, s, cap.saturating_add(1), None, masked)?;
        let cut = if stopped {
            ViaCut::Deletes
        } else if page.len() > cap {
            ViaCut::Cap
        } else {
            ViaCut::None
        };
        let mut ids: Vec<u32> = page.into_iter().take(cap).map(|(_, l)| l).collect();
        ids.sort_unstable();
        ids.dedup();
        Ok((ids, cut))
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

    /// `s`'s lists in `rel` and whether the read was cut, read and counted
    /// once a call.
    fn via_subs<'m>(
        &self,
        cx: &ReadCx<'_>,
        memo: &'m mut Memo,
        rel: RelId,
        s: u32,
        cap: usize,
    ) -> R<(&'m [u32], bool)> {
        let key = (rel, s);
        if !memo.subs.contains_key(&key) {
            let (ids, cut) = self.via_subscriptions(cx, rel, s, cap)?;
            match cut {
                ViaCut::None => {}
                ViaCut::Cap => memo.cuts.cap += 1,
                ViaCut::Deletes => memo.cuts.deletes += 1,
            }
            memo.subs.insert(key, (ids, cut));
        }
        let (ids, cut) = &memo.subs[&key];
        Ok((ids, *cut != ViaCut::None))
    }

    /// Member `m` against a subscriber's sorted `lists`, whose read was `cut`.
    fn via_hit(
        &self,
        cx: &ReadCx<'_>,
        pend: &mut R<()>,
        members: RelId,
        kind: Option<RelId>,
        m: u32,
        (lists, cut): (&[u32], bool),
    ) -> ViaHit {
        let list = item(pend, self.via_match(cx, members, kind, m, lists)).flatten();
        ViaHit { list, truncated: list.is_none() && cut }
    }

    /// Per other and via, in order: the list that answers, or none. An actor
    /// or other that's None (unknown, or hidden) answers none. Every
    /// subscriber's lists are its newest `cap` records'. Out reads the
    /// actor's lists once, at the first other on any list; In reads each
    /// other's lists, unless the actor is on no list at all.
    pub fn relationships_via(
        &self,
        cx: &ReadCx<'_>,
        actor: Option<u32>,
        others: &[Option<u32>],
        vias: &[ViaQuery],
        cap: usize,
    ) -> R<(Vec<Vec<ViaHit>>, ViaCuts)> {
        let mut out = vec![vec![ViaHit::default(); vias.len()]; others.len()];
        let Some(a) = actor else { return Ok((out, ViaCuts::default())) };
        let mut ids: Vec<u32> = others.iter().flatten().copied().collect();
        ids.sort_unstable();
        ids.dedup();
        let mut memo = Memo::default();
        let mut pend = Ok(());
        let mut hits = vec![ViaHit::default(); ids.len()];
        for (vi, v) in vias.iter().enumerate() {
            hits.fill(ViaHit::default());
            match (v.dir, &v.lists) {
                (Dir::Out, lists) => {
                    let mut none = matches!(lists, ViaLists::Given(l) if l.is_empty());
                    for (k, &o) in ids.iter().enumerate() {
                        if none || item(&mut pend, self.via_on(cx, &mut memo, v.members, o)) != Some(true) {
                            continue;
                        }
                        let subs = match lists {
                            ViaLists::Given(l) => (&l[..], false),
                            ViaLists::Rel(r) => match item(&mut pend, self.via_subs(cx, &mut memo, *r, a, cap)) {
                                Some(s) => s,
                                None => continue,
                            },
                        };
                        if subs.0.is_empty() && !subs.1 {
                            // no subscriptions at all: no other needs its count asked
                            none = true;
                            continue;
                        }
                        hits[k] = self.via_hit(cx, &mut pend, v.members, v.kind, o, subs);
                    }
                }
                // a caller's lists are the actor's own: refused for In before here
                (Dir::In, ViaLists::Given(_)) => continue,
                (Dir::In, ViaLists::Rel(r)) => {
                    if item(&mut pend, self.via_on(cx, &mut memo, v.members, a)) != Some(true) {
                        continue;
                    }
                    for (k, &o) in ids.iter().enumerate() {
                        let Some(subs) = item(&mut pend, self.via_subs(cx, &mut memo, *r, o, cap)) else {
                            continue;
                        };
                        hits[k] = self.via_hit(cx, &mut pend, v.members, v.kind, a, subs);
                    }
                }
            }
            for (j, o) in others.iter().enumerate() {
                if let Some(o) = o {
                    out[j][vi] = hits[ids.binary_search(o).unwrap()];
                }
            }
        }
        pend.map(|()| (out, memo.cuts))
    }

    /// Per pair, both ways: a list `a` subscribes to that holds `b`, and one
    /// `b` subscribes to that holds `a`, from `q.rel`'s lists. A pair that's
    /// None (unknown, or hidden) answers neither. Each side's member count
    /// comes before its subscriber's read, and each subscriber is read once
    /// a call, its newest `cap` records, as [`Graph::relationships_via`] reads
    /// it.
    pub fn block_pairs_via(
        &self,
        cx: &ReadCx<'_>,
        pairs: &[Option<(u32, u32)>],
        q: &PairQuery,
        cap: usize,
    ) -> R<(Vec<(ViaHit, ViaHit)>, ViaCuts)> {
        let mut out = vec![(ViaHit::default(), ViaHit::default()); pairs.len()];
        let mut memo = Memo::default();
        let mut pend = Ok(());
        // (subscriber, member)
        let way = |s: u32, m: u32, memo: &mut Memo, pend: &mut R<()>| -> ViaHit {
            if item(pend, self.via_on(cx, memo, q.members, m)) != Some(true) {
                return ViaHit::default();
            }
            match item(pend, self.via_subs(cx, memo, q.rel, s, cap)) {
                Some(subs) => self.via_hit(cx, pend, q.members, q.kind, m, subs),
                None => ViaHit::default(),
            }
        };
        for (p, o) in pairs.iter().zip(&mut out) {
            if let &Some((a, b)) = p {
                *o = (way(a, b, &mut memo, &mut pend), way(b, a, &mut memo, &mut pend));
            }
        }
        pend.map(|()| (out, memo.cuts))
    }
}
