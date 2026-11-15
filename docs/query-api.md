---
title: Query API
section: marco
order: 4
summary: "marco.v1.Graph: Has, Relationships (with list blocks and mutes by via), BlockPairs, ListOut and ListIn, Counts, Intersect, Filter and EdgesAmong. JSON or protobuf over HTTP, gRPC and gRPC-Web, all on one port."
---

```hero
diagram:
  caption: "Every read names a side: a relation, a direction and an actor (or a list). Out is the actor's records, newest first. In is the actors naming a subject, in id order."
  nodes:
    - { id: a, label: actor, sub: "a DID or a list URI", at: [0, 2.5], size: [9, 3], tone: accent }
    - { id: out, label: DIR_OUT, sub: "records · rkey cursor", at: [14, 0], size: [10, 3], tone: blue }
    - { id: inn, label: DIR_IN, sub: "actors · DID cursor", at: [14, 5], size: [10, 3], tone: cyan }
    - { id: ops, label: Intersect · Filter, sub: "and · and-not · drop", at: [29, 2.5], size: [10, 3], tone: solid }
  edges:
    - "a.r -> out.l: ListOut"
    - "a.r -> inn.l: ListIn"
    - "out.r -> ops.l25"
    - "inn.r -> ops.l75"
facts:
  - { value: "9", label: read calls, note: "plus Write and WriteBatch", tone: accent }
  - { value: "100", label: others per Relationships, note: "and actors per Counts", tone: blue }
  - { value: "1,000", label: actors per Filter, note: "and items per ListOut page", tone: cyan }
  - { value: "3", label: protocols on :7300, note: "Connect · gRPC · gRPC-Web" }
```

`marco.v1.Graph` is a ConnectRPC service defined in `proto/marco/v1/graph.proto`. The API listener
(`--listen`, default `0.0.0.0:7300`) answers it as Connect (JSON or protobuf bodies over HTTP POST),
gRPC and gRPC-Web on the same port, so curl works for poking at it and an appview uses generated
clients. Reads are open. Writes need the admin token ([Writing and ingest](ingest.md)). The landing page at `/` on
the API listener has an explorer that runs every read call and shows the `curl` for it.

## Calling it

```bash
curl -s localhost:7300/marco.v1.Graph/Has -H 'content-type: application/json' \
  -d '{"rel":"follow","from":"did:plc:alice2222222222222222222","to":"did:plc:bob222222222222222222222"}'
```

```json
{"uri":"at://did:plc:alice2222222222222222222/app.bsky.graph.follow/3mzjs7ys3uz22"}
```

- JSON fields are lowerCamelCase (`outUri`, `memberOf`). 64-bit numbers come back as strings
  (`"aLen":"2"`), and empty fields and zero counts are left out.
- `rel` takes a relation's name from the [config](relations.md): `follow`, `block`, `listitem`,
  or `listitem.owner`. An unknown one is `invalid_argument`.
- An actor that marco hasn't seen isn't an error. It just has no edges.
- An actor that's taken down or deleted ([statuses](ingest.md#account-statuses)) is hidden: as an
  actor it has no edges, and as a subject it's left out of every list and answer. Counts are the
  exception today. `Counts` and Intersect's `aLen`/`bLen` still count hidden actors, so a follower
  count can include taken-down accounts that `ListIn` leaves out.
- Until marco is ready, every Graph read on the API listener answers `unavailable` with why
  ("catching up: jetstream:…"), unless it runs with `--serve-before-ready`
  ([the ready gate](durability.md#the-ready-gate)). The console's copy of Graph on the admin
  listener always answers.

The examples below use the four accounts from the [Quickstart](quickstart.md).

## Has

Does `from` have a record naming `to`? The answer is the record's URI, or empty.

```json
// Has {"rel":"follow","from":"did:plc:alice2222…","to":"did:plc:bob222…"}
{"uri":"at://did:plc:alice2222222222222222222/app.bsky.graph.follow/3mzjs7ys3uz22"}
```

If `from` has several records naming `to`, it's the newest. For a list relation, `from` is the
list's URI and the answer is the member's oldest listitem.

## Relationships

`getRelationships` in one call: for each of up to 100 `others` and each relation, the actor's
record naming the other (`outUri`) and the other's record naming the actor (`inUri`).

```json
// Relationships {"actor":"did:plc:alice2222…","others":["did:plc:bob222…","did:plc:dave2222…"]}
{"relationships":[
  {"other":"did:plc:bob222222222222222222222","rels":[
    {"rel":"follow","outUri":"at://did:plc:alice…/app.bsky.graph.follow/3mzjs7ys3uz22",
                    "inUri":"at://did:plc:bob…/app.bsky.graph.follow/3mzjs7ys3v322"},
    {"rel":"block"}]},
  {"other":"did:plc:dave22222222222222222222","rels":[
    {"rel":"follow","inUri":"at://did:plc:dave…/app.bsky.graph.follow/3mzjs7ys3v522"},
    {"rel":"block"}]}]}
```

Answers come in the request's order. `rels` empty means every relation between actors (`follow`
and `block` by default), which leaves out list relations and relations whose subjects are records
([Records as subjects](relations.md#records-as-subjects)). This is the call a profile's viewer state and a
feed's hydration make. 50 others × 2 relations take ~30 µs in the engine and ~100 µs over HTTP/2
([Throughput and latency](perf.html)).

## Via

```diagram
caption: "A list block is two hops that meet on the list's id. `via` reads the subscriber's lists, then probes the member's in set (the lists it's on) with them. The answer is the list's URI."
nodes:
  - { id: s, label: subscriber, sub: "the actor (DIR_OUT)", at: [0, 0], size: [9, 3], tone: blue }
  - { id: l, label: list, sub: "AT URI node", at: [15, 0], size: [9, 3], tone: accent }
  - { id: m, label: member, sub: "each other (DIR_OUT)", at: [30, 0], size: [9, 3], tone: cyan }
  - { id: o, label: owner, sub: "mod list while it holds", at: [15, 5], size: [9, 3], tone: muted }
edges:
  - "s.r -> l.l: listblock"
  - "l.r -> m.l: listitem"
  - "o.t -> l.b: modlist"
```

A list block or a list mute is two hops. The viewer subscribes to a list, and the list holds the
other. `via` asks that in the same `Relationships` call, one direction per entry, and each other
gets one answer per entry, in order, in its `via`. The relations it reads are config
([Records as subjects](relations.md#records-as-subjects)): `listblock` for subscriptions to block
lists, `listitem` for the members, and `modlist` to count mod lists only.

The example is the [Quickstart's list blocks](quickstart.md#list-blocks): alice subscribes to
carol's mod list, which holds dave, and the third entry passes alice's own list (which holds bob
and dave) the way a caller passes a viewer's mute lists.

```json
// Relationships {"actor":"did:plc:alice2222…","others":["did:plc:bob222…","did:plc:dave2222…"],
//   "rels":["block"],
//   "via":[{"rel":"listblock","members":"listitem","kind":"modlist","dir":"DIR_OUT"},
//          {"rel":"listblock","members":"listitem","kind":"modlist","dir":"DIR_IN"},
//          {"lists":{"uris":["at://did:plc:alice2222…/app.bsky.graph.list/3n…"]},"members":"listitem",
//           "dir":"DIR_OUT"}]}
{"relationships":[
  {"other":"did:plc:bob222222222222222222222","rels":[{"rel":"block"}],
   "via":[{},{},{"list":"at://did:plc:alice2222222222222222222/app.bsky.graph.list/3n…"}]},
  {"other":"did:plc:dave22222222222222222222","rels":[{"rel":"block"}],
   "via":[{"list":"at://did:plc:carol2222222222222222222/app.bsky.graph.list/3mzmods222222"},{},
          {"list":"at://did:plc:alice2222222222222222222/app.bsky.graph.list/3n…"}]}]}
```

| Field | |
|---|---|
| `rel` | A relation whose subjects are records, such as `listblock` (it has a `subject_collection`). The subscriber's lists are the subjects of its newest records in it. |
| `lists` | In place of `rel`, `{"uris":[…]}`: up to 1,000 list URIs the caller holds for the actor. Some subscriptions, such as list mutes, are private state rather than records in a repo, so marco doesn't have them and the caller passes them in. marco doesn't check that the lists are the actor's. `DIR_OUT` only. |
| `members` | Required. The list relation whose members count (`listitem`). |
| `kind` | Optional. A filter on which lists count: only a list that a live record of this relation names, one whose actor isn't taken down or deleted. With `modlist`, a list counts while it's a mod list. Left out, any list counts. |
| `dir` | Required. `DIR_OUT` asks whether the actor's lists hold the other, and `DIR_IN` whether the other's lists hold the actor. |

An answer's `list` is one matching list's URI, or left out when none matches. It's the same list
from call to call while the lists and subscriptions don't change. A list whose owner is taken down
or deleted never matches, with or without `kind`. A hidden or unknown actor or other gets an empty
answer for every entry, so each `Relationship` carries exactly one answer per entry. A request
without `via` gets no answers at all.

### What a via reads

Every subscriber's lists are its newest 1,000 records in `rel` (`--via-max-records`), whichever
call, direction or pair names it. So `Relationships` from either side and `BlockPairs` agree about
the same two accounts. When a read stops at the cap and the answer is empty, that answer's
`truncated` is true: a list further back may still hold the other. An answer that found a list
isn't truncated, and neither is one from `lists`, which takes at most 1,000 URIs or the request is
`invalid_argument`.

marco checks the member's list count first, one lookup, so a member on no list (most accounts)
costs no read of the subscriber. Each subscriber is read once a call, however many others or pairs
name it. Its lists, sorted, probe the member's in set (the lists it's on), which is never decoded.
So a call costs at most one read of 1,000 records per distinct subscriber, whatever the accounts
it names subscribe to.

A read skips records that writes since the last checkpoint deleted or replaced, and stops after
passing 10 of them per record of the cap (10,000 by default). An account that just dropped more
subscriptions than that is read only in part until the next checkpoint folds the deletes, and its
empty answers say they're truncated.

Every read that stops short counts in `marco_via_truncated_total{rpc, reason}`: `"cap"` for an
account with more records than the cap, and `"deletes"` for one with more recent deletes than a
read passes.

## BlockPairs

Per pair of up to 1,000, whether `a` has a record naming `b` (`aBlocksB`), the reverse
(`bBlocksA`), and either (`blocked`). `rels` names the relations that count, `block` by default,
and refuses list relations and relations whose subjects are records. A DID that's unknown, taken
down or deleted blocks nobody and is blocked by nobody. This is how a thread drops the replies its
viewer can't interact with.

```json
// BlockPairs {"pairs":[{"a":"did:plc:alice2222…","b":"did:plc:dave2222…"},
//                      {"a":"did:plc:dave2222…","b":"did:plc:carol2222…"}],"uris":true,
//   "via":{"rel":"listblock","members":"listitem","kind":"modlist"}}
{"aBlocksB":[false,false],"bBlocksA":[false,true],"blocked":[true,true],
 "aBlocksBUri":["",""],"bBlocksAUri":["","at://did:plc:carol2222222222222222222/app.bsky.graph.block/3n…"],
 "aBlocksBList":["at://did:plc:carol2222222222222222222/app.bsky.graph.list/3mzmods222222",""],
 "bBlocksAList":["",""],"aBlocksBTruncated":[false,false],"bBlocksATruncated":[false,false]}
```

`uris` adds the URIs of the records behind `aBlocksB` and `bBlocksA` (`aBlocksBUri`,
`bBlocksAUri`), the newest in any of `rels` if a side has several, one lookup per hit.

`via` adds blocks by list in both directions. It takes a `rel`, `members` and `kind` as a
[Via](#via) does, and no `dir`, since each pair is checked both ways. Each pair gets a list `a`
subscribes to that holds `b` (`aBlocksBList`), one `b` subscribes to that holds `a`
(`bBlocksAList`), and a truncation flag for each, as a via answer's `truncated`. `blocked` counts the lists
too, and `aBlocksB` and `bBlocksA` stay the direct records. A side's lists are read as in
[What a via reads](#what-a-via-reads): after its member's list count says it's on a list, and once
a call however many pairs name it.

## ListOut and ListIn

```json
// ListOut {"rel":"follow","actor":"did:plc:alice2222…","limit":1}
{"edges":[{"uri":"at://did:plc:alice…/app.bsky.graph.follow/3mzjs7ys3v222",
           "subject":"did:plc:carol2222222222222222222"}],
 "cursor":"3mzjs7ys3v222"}

// ListIn {"rel":"follow","subject":"did:plc:alice2222…","limit":1,"cursor":"did:plc:bob222…"}
{"actors":["did:plc:carol2222222222222222222"],"cursor":"did:plc:carol2222222222222222222"}
```

| | ListOut | ListIn |
|---|---|---|
| What | the actor's records: URI and subject | the actors with a record naming the subject |
| Order | newest first (rkey order is time order) | by id: sorted DIDs from the build, then arrival order |
| Cursor | an rkey: the records older than it | a DID: the actors after it |
| A bad cursor | one that isn't an rkey is hashed to an arbitrary position | a DID marco doesn't know returns an empty page and no cursor, which looks like the end |
| `limit` | default 50, at most 1,000 | default 50, at most 1,000 |
| Extra | `distinct`: one record per subject, its oldest | |

Pass the last response's `cursor` to get the next page. A full page always carries a cursor, so a
last page that's exactly full has one too, and the call after it returns an empty page with no
cursor. A page can hold fewer than `limit` items before the end, since hidden actors are dropped
after the page is cut, so page until the cursor is empty. Records whose rkey isn't a TID sort as newest.
Neither call rejects a bad cursor (Intersect does), so pass back only cursors marco gave you.

## Counts

Per actor (up to 100) and relation: `outRecords` (the actor's records), `inActors` (the actors
naming it) and `outSubjects` (the distinct subjects of its records, which is a list's member
count).

```json
// Counts {"actors":["did:plc:alice2222…"],"rels":["follow"]}
{"counts":[{"actor":"did:plc:alice2222222222222222222",
            "rels":[{"rel":"follow","outRecords":"2","inActors":"3","outSubjects":"2"}]}]}
```

`rels` empty means every relation, list relations included. `outRecords` and `outSubjects`
differ when an actor has two records naming the same subject (about 0.14% of follows). Hidden
actors are counted in all three.

## Intersect

Two sides, each a `(rel, dir, actor)`, combined with `SET_OP_AND` (the default) or
`SET_OP_AND_NOT` (in `a` and not in `b`). Mutuals are your follows and your followers:

```json
// Intersect {"a":{"rel":"follow","dir":"DIR_OUT","actor":"did:plc:alice2222…"},
//            "b":{"rel":"follow","dir":"DIR_IN","actor":"did:plc:alice2222…"}}
{"actors":["did:plc:bob222222222222222222222","did:plc:carol2222222222222222222"],"aLen":"2","bLen":"3"}
```

| Question | a | b | op |
|---|---|---|---|
| Mutuals | `follow`, OUT, viewer | `follow`, IN, viewer | AND |
| Followed by people you follow | `follow`, OUT, viewer | `follow`, IN, author | AND |
| Followers not on a list | `follow`, IN, actor | `listitem`, OUT, list URI | AND_NOT |
| On both lists | `listitem`, OUT, list 1 | `listitem`, OUT, list 2 | AND |

- Actors come back in id order. `aLen` and `bLen` are the two sides' lengths as `Counts` would
  give them: an out side counts records (an actor following someone twice counts twice), an in
  side counts actors, and hidden actors are included.
- `limit`: default 100, at most 10,000. A full page carries a `cursor` (the last actor's DID), and
  passing it back gets the actors after it.
- `count`: also return `total`, the size of the whole result, whatever the page or cursor, for
  "followed by A, B and N others". It's exact by default. The page doesn't change, and nothing is
  counted unless asked. The count is one pass over the side the page already decodes (the smaller
  one for AND, `a` for AND_NOT), so it costs no more than that side's length.
- `count_limit`: cap the total at this many (at most 10,000). Past it `totalCapped` is true and
  `total` is the cap. Leave it out, or send 0, for an exact total. `total` includes actors the page
  leaves out as taken down or deleted.

```json
// Intersect {"a":{"rel":"follow","dir":"DIR_IN","actor":"did:plc:carol2222…"},
//            "b":{"rel":"follow","dir":"DIR_OUT","actor":"did:plc:alice2222…"},
//            "limit":5,"count":true}
{"actors":["did:plc:bob222222222222222222222"],"aLen":"3","bLen":"2","total":"1"}
```

- A cursor that isn't a DID in the graph is `invalid_argument`. A side whose actor marco doesn't
  know, or that's hidden, is empty, so AND gives nothing and AND_NOT gives all of `a`.
- The cost is the smaller side: a random viewer's mutuals take ~0.6 µs on the engine, and an
  account following 886k takes ~8 ms ([Throughput and latency](perf.html)).

## Filter

`actors` (up to 1,000) less those a drop rule hits for `viewer`, in the request's order. This is
how a feed or thread drops blocked accounts and a list's members.

```json
// Filter {"viewer":"did:plc:dave2222…","actors":["…alice…","…bob…","…carol…"],
//         "drop":[{"rel":"block","dir":"DIR_OUT"},{"rel":"block","dir":"DIR_IN"}]}
{"actors":["did:plc:alice2222222222222222222","did:plc:bob222222222222222222222"],
 "dropped":[{"actor":"did:plc:carol2222222222222222222","rel":"block","dir":"DIR_IN"}]}
```

| Drop rule | Drops |
|---|---|
| `{"rel":"block","dir":"DIR_OUT"}` | actors the viewer blocks |
| `{"rel":"block","dir":"DIR_IN"}` | actors blocking the viewer |
| `{"memberOf":{"rel":"listitem","dir":"DIR_OUT","actor":"<list URI>"}}` | the list's members, whoever the viewer is |
| `{"via":{"rel":"listblock","members":"listitem","kind":"modlist","dir":"DIR_OUT"}}` | actors on a mod list the viewer subscribes to |
| `{"via":{"rel":"listblock","members":"listitem","kind":"modlist","dir":"DIR_IN"}}` | actors subscribed to a mod list that holds the viewer |
| `{"via":{"lists":{"uris":["<list URI>"]},"members":"listitem","dir":"DIR_OUT"}}` | actors on the viewer's own lists, such as its mute lists |

`dropped` names the rule that hit each actor, or `"rel":"status"` for one that's taken down or
deleted. When several rules hit, a `rel`/`dir` rule is reported before any `memberOf` rule, and
both before a `via` rule, whatever order the request lists them in. Actors marco doesn't know are
kept. A viewer marco doesn't know has no blocks or subscriptions, so only `memberOf` rules apply.
An unknown or hidden list drops nobody.

A `via` rule is a [Via](#via) with the viewer as its actor, read the same way and with the same
cap ([What a via reads](#what-a-via-reads)). Its drop names the via's `rel` (or `members`, for
`lists`) and `dir`, and the list that matched in `list`. A kept actor that a via rule may have
missed, because the subscriber's read stopped short, is also listed in `viaTruncated`. At most 10
`via` rules a request.

## EdgesAmong

```diagram
caption: "Each edge is found once. A member with a short out log scans it against the set. One whose out log is long next to the set is probed: every member's in set is asked whether it holds it."
nodes:
  - { id: set, label: actors, sub: "up to 10,000 DIDs", at: [0, 2.5], size: [9, 3], tone: accent }
  - { id: scan, label: scan, sub: "out log · hash of the set", at: [14, 0], size: [10, 3], tone: blue }
  - { id: probe, label: probe, sub: "in sets · long out logs", at: [14, 5], size: [10, 3], tone: cyan }
  - { id: res, label: edges, sub: "from · rel · to · uri", at: [29, 2.5], size: [10, 3], tone: solid }
edges:
  - "set.r -> scan.l: short"
  - "set.r -> probe.l: long"
  - "scan.r -> res.l25"
  - "probe.r -> res.l75"
```

Every edge whose actor and subject are both in `actors`: who in a set follows (or blocks) whom.
A farm of accounts that rarely follow each other and a circle where everyone follows everyone look
the same one account at a time, and this tells them apart in one call.

```bash
curl -s localhost:7300/marco.v1.Graph/EdgesAmong -H 'content-type: application/json' \
  -d '{"actors":["did:plc:alice2222222222222222222","did:plc:bob222222222222222222222",
                 "did:plc:carol2222222222222222222","did:plc:erin2222222222222222222"]}'
```

```json
{"edges":[
  {"from":"did:plc:alice2222222222222222222","rel":"follow","to":"did:plc:bob222222222222222222222",
   "uri":"at://did:plc:alice2222222222222222222/app.bsky.graph.follow/3mzjs7ys3uz22"},
  {"from":"did:plc:alice2222222222222222222","rel":"follow","to":"did:plc:carol2222222222222222222",
   "uri":"at://did:plc:alice2222222222222222222/app.bsky.graph.follow/3mzjs7ys3v222"},
  {"from":"did:plc:bob222222222222222222222","rel":"follow","to":"did:plc:alice2222222222222222222",
   "uri":"at://did:plc:bob222222222222222222222/app.bsky.graph.follow/3mzjs7ys3v322"},
  {"from":"did:plc:carol2222222222222222222","rel":"follow","to":"did:plc:alice2222222222222222222",
   "uri":"at://did:plc:carol2222222222222222222/app.bsky.graph.follow/3mzjs7ys3v422"}],
 "counts":[{"rel":"follow","edges":"4"},{"rel":"block"}],
 "unknown":["did:plc:erin2222222222222222222"]}
```

- One edge per (actor, subject) pair and relation, with the URI of its newest record, as `Has`
  gives it.
- `counts` has every relation asked, in order: the edges among the actors, past `limit` too.
- `unknown` is the actors marco has no record of, in the request's order. A repeated actor counts
  once.
- `rels` empty means the relations between actors (`follow` and `block` by default). A list
  relation, its `.owner` companion and a relation whose subjects are records (`listblock`,
  `modlist`) are refused with `invalid_argument`, since their actors or subjects aren't DIDs.
- An actor that's taken down or deleted has no edges either way, and isn't in `unknown`.
- Edges come by relation in the order asked, then by actor and subject in id order. `limit`
  (default 10,000, at most 50,000) caps them, and past it `truncated` is true. A truncated answer
  holds the first relation's edges first, so ask one relation per call to get some of each.

A subscription to a mod list (`listblock`) or a mod list itself (`modlist`) names a list, not an
account. Covering them would join through the list: the edges from the set's members to lists,
then each list's members (`listitem`) in the set, as "a blocks b through list L". That's a
second kind of edge, with the list as its middle, so it would be a field of its own. For one
account against others, or pair by pair, `Relationships` and `BlockPairs` make that join with
[`via`](#via).

### How it's answered

Per relation, each member is scanned or probed by its out log's length. A member that follows
fewer than 64 times the set's size scans its out log, testing each subject against a hash of the
set, and reads its rkeys only for the edges it returns. A member with a longer out log is probed:
each member's in set is asked, block by block, which of the probed members it holds, and the record
of each hit is found through the probed member's by-subject index. An edge from a scanned member
is found by its scan, and one from a probed member by its subject's in set, so none is found
twice. A small set of popular accounts, each following thousands, is mostly probed, and a big set
mostly scans.

Measured in the engine on the whole network's follows and blocks (a 47 GB snapshot, warm, on a
32-thread box), both relations asked, DIDs resolved and URIs built. Sets are drawn from the 20,000
accounts with the most followers, from accounts that follow and are followed by at most 500, and
from one account's followers. p99 is over 200 sets of 5 and 50, 40 of 500 and 10 of 5,000.

| Set | Follows among them (mean) | p50 | p99 |
|---|---|---|---|
| 5 popular | 3 | 5 µs | 34 µs |
| 50 popular | 359 | 0.6 ms | 2.1 ms |
| 500 popular | 34,488 | 27 ms | 34 ms |
| 5,000 popular | 2,776,227 | 250 ms | 264 ms |
| 5,000 ordinary | 4 | 4.1 ms | 8.0 ms |
| 50 followers of one account | 247 | 0.25 ms | 2.4 ms |
| 500 followers of one account | 23,763 | 16 ms | 56 ms |
| 5,000 followers of one account | 1,873,276 | 57 ms | 374 ms |

Probing keeps small sets cheap: scanning every member of a set of 5 popular accounts takes 95 µs.
Scanning keeps big ones cheap: probing every member of 5,000 takes 1.2 s. A request of more than 100 actors runs
on the miss pool from the start, so a big set never holds a worker.

A request runs for at most 5 seconds, or its client's deadline if sooner, and past that is
`deadline_exceeded`. Like every read, one that needs more of the cache at once than a request may
hold is `resource_exhausted`.

## Lists

A list relation's actor is the list's AT URI, so every call above takes a list:

| Lookup | Call |
|---|---|
| A list's members, newest first | `ListOut {"rel":"listitem","actor":"<list URI>","distinct":true}` |
| The lists a DID is on | `ListIn {"rel":"listitem","subject":"<did>"}`: list URIs, in id order |
| Is a DID on a list | `Has {"rel":"listitem","from":"<list URI>","to":"<did>"}`: its oldest listitem |
| Member count | `Counts {"actors":["<list URI>"],"rels":["listitem"]}`: `outSubjects` |
| The owner's listitems | `ListOut {"rel":"listitem.owner","actor":"<owner DID>"}`: each item's URI, and its list as the subject |
| Two lists, or a list and a relation | `Intersect` with a `listitem` side |
| Drop a list's members | `Filter` with `memberOf` |
| Drop the accounts on a viewer's block or mute lists | `Filter` with `via` |

`distinct` keeps each member's oldest record, as the appview does, so a repeat added later can't
move a member past your cursor. The [Relations](relations.md#lists) page has how lists are stored.

## Limits and errors

| Call | Limit |
|---|---|
| `Relationships` | 100 others · 10 `via` entries · 1,000 `lists` an entry |
| `BlockPairs` | 1,000 pairs |
| `Counts` | 100 actors |
| `ListOut`, `ListIn` | `limit` default 50, at most 1,000 |
| `Intersect` | `limit` default 100, at most 10,000 |
| `Filter` | 1,000 actors · 10 `via` rules |
| `EdgesAmong` | 10,000 actors (`--edges-among-max-actors`), `limit` default 10,000, at most 50,000 |
| `Write`, `WriteBatch` | 1,000 ops a message |

A request over a limit is `invalid_argument`, except `limit` itself, which is clamped. Errors use
Connect's codes: `invalid_argument` for a bad request, `unavailable` before ready,
`deadline_exceeded` for an `EdgesAmong` past its deadline,
`unauthenticated` for a write without the token, `failed_precondition` for a write to a
`--read-only` marco.
