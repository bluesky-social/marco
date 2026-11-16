---
title: Query API
section: marco
order: 4
summary: "marco.v1.Graph: Has, Relationships, ListOut and ListIn, Counts, Intersect, Filter and EdgesAmong. JSON or protobuf over HTTP, gRPC and gRPC-Web, all on one port."
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

`dropped` names the rule that hit each actor, or `"rel":"status"` for one that's taken down or
deleted. When several rules hit, a `rel`/`dir` rule is reported before any `memberOf` rule,
whatever order the request lists them in. Actors marco doesn't know are kept. A viewer marco doesn't know has no blocks, so only
`memberOf` rules apply. An unknown or hidden list drops nobody.

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
second kind of edge, with the list as its middle, so it would be a field of its own.

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

`distinct` keeps each member's oldest record, as the appview does, so a repeat added later can't
move a member past your cursor. The [Relations](relations.md#lists) page has how lists are stored.

## Limits and errors

| Call | Limit |
|---|---|
| `Relationships` | 100 others |
| `Counts` | 100 actors |
| `ListOut`, `ListIn` | `limit` default 50, at most 1,000 |
| `Intersect` | `limit` default 100, at most 10,000 |
| `Filter` | 1,000 actors |
| `EdgesAmong` | 10,000 actors (`--edges-among-max-actors`), `limit` default 10,000, at most 50,000 |
| `Write`, `WriteBatch` | 1,000 ops a message |

A request over a limit is `invalid_argument`, except `limit` itself, which is clamped. Errors use
Connect's codes: `invalid_argument` for a bad request, `unavailable` before ready,
`deadline_exceeded` for an `EdgesAmong` past its deadline,
`unauthenticated` for a write without the token, `failed_precondition` for a write to a
`--read-only` marco.
