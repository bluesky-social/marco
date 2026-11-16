//! Relationships from config over a server: the default set, a block by mod
//! list both ways, and an operator's private mutes and mute lists, fed only
//! through the write API, in Relationships, BlockPairs and Filter.

use std::net::SocketAddr;
use std::sync::Arc;

use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::ErrorCode;
use marco_proto::connect::marco::v1::{AdminClient, GraphClient};
use marco_proto::v1::__buffa::oneof::{write_op, write_result};
use marco_proto::v1::*;
use marco_server::app::{App, Config};
use marco_server::auth::AuthConfig;
use marco_server::synth::{did, list_uri, tid, uri, BLOCK, FOLLOW, LISTITEM};

const TOKEN: &str = "test-token-0123456789";
const LISTBLOCK: &str = "app.bsky.graph.listblock";
const MUTE: &str = "com.example.graph.mute";
const LISTMUTE: &str = "com.example.graph.listmute";

/// Bluesky's graph relations, then two an operator writes from its own
/// state: an actor's mutes and its mute-list subscriptions.
const RELATIONS: &str = r#"
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

[[relation]]
name = "mute"
collection = "com.example.graph.mute"
subject = "subject"
write_only = true

[[relation]]
name = "listmute"
collection = "com.example.graph.listmute"
subject = "subject"
subject_collection = "app.bsky.graph.list"
write_only = true
"#;

/// The default's three, then the mutes: only the actor's side of each.
const RELATIONSHIPS: &str = r#"
[[relationship]]
name = "follow"
kind = "direct"
relation = "follow"

[[relationship]]
name = "block"
kind = "direct"
relation = "block"
blocks = true

[[relationship]]
name = "block_by_list"
kind = "list"
subscription = "listblock"
membership = "listitem"
list_filter = "modlist"
blocks = true

[[relationship]]
name = "mute"
kind = "direct"
relation = "mute"
private = true

[[relationship]]
name = "mute_by_list"
kind = "list"
subscription = "listmute"
membership = "listitem"
list_filter = "modlist"
private = true
"#;

struct Srv {
    api: SocketAddr,
    admin: SocketAddr,
    _dir: tempfile::TempDir,
}

fn snapshot(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let snap = dir.path().join("snap");
    let mut g = marco_core::Graph::new(marco_core::Relations::parse_toml(RELATIONS).unwrap());
    g.add(&uri(&did(0), FOLLOW, &tid(1, 0)), &did(1)).unwrap();
    g.write_snapshot(&snap).unwrap();
    snap
}

async fn start(relationships: Option<&str>) -> Srv {
    let dir = tempfile::tempdir().unwrap();
    let snap = snapshot(&dir);
    let auth = AuthConfig { token: Some(TOKEN.into()), ..Default::default() };
    let cfg = Config { auth, relationships: relationships.map(str::to_string), ..Config::new(snap) };
    let app = App::open(cfg).unwrap();
    let ui = Arc::new(marco_server::ui::Ui::load(None).unwrap());
    let (api, admin) = marco_server::serve(app, ui).await.unwrap();
    Srv { api, admin, _dir: dir }
}

fn cfg(addr: SocketAddr) -> ClientConfig {
    ClientConfig::new(format!("http://{addr}").parse().unwrap()).with_default_header(
        http::header::AUTHORIZATION,
        format!("Bearer {TOKEN}").parse::<http::HeaderValue>().unwrap(),
    )
}

fn add(u: &str, s: &str) -> WriteOp {
    add_in(u, s, "")
}

fn add_in(u: &str, s: &str, list: &str) -> WriteOp {
    let a = AddOp { uri: u.into(), subject: s.into(), list: list.into(), ..Default::default() };
    WriteOp { op: Some(write_op::Op::Add(Box::new(a))), ..Default::default() }
}

/// Per other: (rel, out_uri, in_uri) for every relationship answered.
fn edges(r: &RelationshipsResponse) -> Vec<Vec<(String, String, String)>> {
    r.relationships
        .iter()
        .map(|x| x.rels.iter().map(|e| (e.rel.clone(), e.out_uri.clone(), e.in_uri.clone())).collect())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relationships_from_config() {
    let s = start(Some(RELATIONSHIPS)).await;
    let g = GraphClient::new(HttpClient::plaintext(), cfg(s.api));
    // V the viewer, B and C on O's mod list, D on O's curation list, E on a
    // mod list D owns and subscribes to
    let (v, b, c, d, o) = (did(10), did(11), did(12), did(13), did(14));
    let (ml, cur, mutes, dl) = (list_uri(&o, 1), list_uri(&o, 2), list_uri(&o, 3), list_uri(&d, 1));
    let mut k = 0u64;
    let mut key = || {
        k += 1;
        tid(k, 0)
    };
    let item = |owner: &str, rkey: &str, m: &str, l: &str| add_in(&uri(owner, LISTITEM, rkey), m, l);
    let v_mutes_c = uri(&v, MUTE, &c);
    let ops = vec![
        item(&o, &key(), &b, &ml),
        item(&o, &key(), &c, &ml),
        add(&ml, &ml),
        item(&o, &key(), &d, &cur),
        item(&o, &key(), &b, &mutes),
        add(&mutes, &mutes),
        item(&d, &key(), &v, &dl),
        add(&dl, &dl),
        add(&uri(&v, LISTBLOCK, &key()), &ml),
        add(&uri(&d, LISTBLOCK, &key()), &dl),
        // the operator's: V mutes C, C mutes V, V's mute lists are O's mod
        // list of mutes and the curation list, which a mute by list skips
        add(&v_mutes_c, &c),
        add(&uri(&c, MUTE, &v), &v),
        add(&uri(&v, LISTMUTE, &key()), &mutes),
        add(&uri(&v, LISTMUTE, &key()), &cur),
        add(&uri(&b, BLOCK, &key()), &c),
    ];
    let r = g.write_batch(WriteBatchRequest { ops, ..Default::default() }).await.unwrap().into_owned();
    assert!(r.results.iter().all(|x| matches!(x.outcome, Some(write_result::Outcome::Added(_)))), "{r:?}");

    let req = |actor: &str, rels: &[&str]| RelationshipsRequest {
        actor: actor.into(),
        others: vec![b.clone(), c.clone(), d.clone()],
        rels: rels.iter().map(|r| r.to_string()).collect(),
        ..Default::default()
    };
    let e = String::new;
    let row = |f: [String; 2], bl: [String; 2], lb: [String; 2], m: [String; 2], ml_: [String; 2]| {
        [("follow", f), ("block", bl), ("block_by_list", lb), ("mute", m), ("mute_by_list", ml_)]
            .map(|(n, [o, i])| (n.to_string(), o, i))
            .to_vec()
    };
    let rs = g.relationships(req(&v, &[])).await.unwrap().into_owned();
    assert_eq!(
        edges(&rs),
        [
            row([e(), e()], [e(), e()], [ml.clone(), e()], [e(), e()], [mutes.clone(), e()]),
            row([e(), e()], [e(), e()], [ml.clone(), e()], [v_mutes_c.clone(), e()], [e(), e()]),
            row([e(), e()], [e(), e()], [e(), dl.clone()], [e(), e()], [e(), e()]),
        ],
        "every default relationship, in config order: C's mute of V and D on a curation list aren't there"
    );
    assert!(rs.relationships.iter().all(|x| x.via.is_empty() && x.rels.iter().all(|r| !r.out_truncated)));

    // the other side sees its own mute, and nothing of V's
    let rc = g.relationships(RelationshipsRequest { others: vec![v.clone()], ..req(&c, &["mute", "block_by_list"]) });
    let rc = rc.await.unwrap().into_owned();
    assert_eq!(edges(&rc), [vec![("mute".into(), uri(&c, MUTE, &v), e()), ("block_by_list".into(), e(), ml.clone())]]);

    // a relation by name answers as it always did, both ways
    let rf = g.relationships(req(&v, &["follow", "listitem"])).await.unwrap().into_owned();
    assert_eq!(rf.relationships[0].rels.iter().map(|r| r.rel.as_str()).collect::<Vec<_>>(), ["follow", "listitem"]);
    let bad = g.relationships(req(&v, &["muting"])).await.unwrap_err();
    assert_eq!(bad.code, ErrorCode::InvalidArgument);

    // BlockPairs: the relationships that block, both ways, mutes not among them
    let pairs: Vec<BlockPair> = [(&v, &b), (&d, &v), (&v, &c), (&b, &c), (&c, &d)]
        .map(|(a, b)| BlockPair { a: a.clone(), b: b.clone(), ..Default::default() })
        .into();
    let bp = |rels: &[&str]| BlockPairsRequest {
        pairs: pairs.clone(),
        rels: rels.iter().map(|r| r.to_string()).collect(),
        ..Default::default()
    };
    let r = g.block_pairs(bp(&[])).await.unwrap().into_owned();
    assert_eq!(r.blocked, [true, true, true, true, false]);
    assert_eq!(r.a_blocks_b, [false, false, false, true, false], "the direct block");
    assert_eq!(r.a_blocks_b_list, [ml.clone(), dl.clone(), ml.clone(), e(), e()]);
    assert_eq!(r.b_blocks_a_list, [e(), e(), e(), e(), e()]);
    let r = g.block_pairs(bp(&["block"])).await.unwrap().into_owned();
    assert_eq!((r.blocked, r.a_blocks_b_list), (vec![false, false, false, true, false], vec![]), "named, block alone");
    for private in ["mute", "mute_by_list"] {
        let err = g.block_pairs(bp(&[private])).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{private}");
    }

    // Filter: a relationship by list as a drop rule, out and in
    let filter = |rel: &str, dir: Dir| FilterRequest {
        viewer: v.clone(),
        actors: vec![b.clone(), c.clone(), d.clone()],
        drop: vec![FilterDrop { rel: rel.into(), dir: dir.into(), ..Default::default() }],
        ..Default::default()
    };
    let kept = |r: FilterResponse| r.actors;
    assert_eq!(
        kept(g.filter(filter("block_by_list", Dir::DIR_OUT)).await.unwrap().into_owned()),
        std::slice::from_ref(&d)
    );
    assert_eq!(
        kept(g.filter(filter("block_by_list", Dir::DIR_IN)).await.unwrap().into_owned()),
        [b.clone(), c.clone()]
    );
    assert_eq!(
        kept(g.filter(filter("mute_by_list", Dir::DIR_OUT)).await.unwrap().into_owned()),
        [c.clone(), d.clone()]
    );
    let err = g.filter(filter("mute_by_list", Dir::DIR_IN)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument, "the other's mute lists aren't the viewer's to see");

    // the console's view of the config
    let a = AdminClient::new(HttpClient::plaintext(), cfg(s.admin));
    let rl = a.list_relations(ListRelationsRequest::default()).await.unwrap().into_owned();
    let names: Vec<(&str, &str, bool, bool)> =
        rl.relationships.iter().map(|r| (r.name.as_str(), r.kind.as_str(), r.blocks, r.private)).collect();
    assert_eq!(
        names,
        [
            ("follow", "direct", false, false),
            ("block", "direct", true, false),
            ("block_by_list", "list", true, false),
            ("mute", "direct", false, true),
            ("mute_by_list", "list", false, true),
        ]
    );
    assert!(rl.relationships_config.contains("subscription = \"listmute\""), "{}", rl.relationships_config);
    let wo: Vec<&str> = rl.relations.iter().filter(|r| r.write_only).map(|r| r.name.as_str()).collect();
    assert_eq!(wo, ["mute", "listmute"]);
}

/// Without --relationships: follow, block and block_by_list, since the
/// snapshot has their relations. A config that names a relation it lacks
/// doesn't start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_and_a_bad_config() {
    let s = start(None).await;
    let g = GraphClient::new(HttpClient::plaintext(), cfg(s.api));
    let req = RelationshipsRequest { actor: did(0), others: vec![did(1)], ..Default::default() };
    let rs = g.relationships(req).await.unwrap().into_owned();
    let names: Vec<&str> = rs.relationships[0].rels.iter().map(|r| r.rel.as_str()).collect();
    assert_eq!(names, ["follow", "block", "block_by_list"]);
    assert_eq!(rs.relationships[0].rels[0].out_uri, uri(&did(0), FOLLOW, &tid(1, 0)));

    let dir = tempfile::tempdir().unwrap();
    let bad = "[[relationship]]\nname = 'x'\nkind = 'direct'\nrelation = 'repost'\n";
    let cfg = Config { relationships: Some(bad.into()), ..Config::new(snapshot(&dir)) };
    let err = format!("{:#}", App::open(cfg).err().expect("refused"));
    assert!(err.contains("--relationships") && err.contains("repost"), "{err}");
}
