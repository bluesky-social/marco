//! Graph/EdgesAmong on every read listener of a read-only marco: the API
//! listener, the LAN one (`--listen-extra`) and the admin listener.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;

use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::ErrorCode;
use marco_core::{Graph, Relations, Status};
use marco_proto::connect::marco::v1::GraphClient;
use marco_proto::v1::*;
use marco_server::app::{App, Config};
use marco_server::synth::{did, tid, uri, BLOCK, FOLLOW};

const RELS: &str = r#"
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
"#;

fn graph(addr: SocketAddr) -> GraphClient<HttpClient> {
    GraphClient::new(HttpClient::plaintext(), ClientConfig::new(format!("http://{addr}").parse().unwrap()))
}

fn free_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string()
}

fn req(actors: &[String], rels: &[&str], limit: u32) -> EdgesAmongRequest {
    EdgesAmongRequest {
        actors: actors.to_vec(),
        rels: rels.iter().map(|r| r.to_string()).collect(),
        limit,
        ..Default::default()
    }
}

fn triples(r: &EdgesAmongResponse) -> BTreeSet<(String, String, String)> {
    r.edges.iter().map(|e| (e.from.clone(), e.rel.clone(), e.to.clone())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edges_among_on_every_read_listener() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("snap");
    let d: Vec<String> = (0..7).map(did).collect();
    let mut g = Graph::new(Relations::parse_toml(RELS).unwrap());
    let f01 = uri(&d[0], FOLLOW, &tid(1, 0));
    g.add(&f01, &d[1]).unwrap();
    g.add(&uri(&d[1], FOLLOW, &tid(2, 0)), &d[0]).unwrap();
    g.add(&uri(&d[0], FOLLOW, &tid(3, 0)), &d[2]).unwrap();
    let gone = uri(&d[2], FOLLOW, &tid(4, 0));
    g.add(&gone, &d[1]).unwrap();
    g.add(&uri(&d[2], FOLLOW, &tid(5, 0)), &d[4]).unwrap();
    g.add(&uri(&d[2], BLOCK, &tid(6, 0)), &d[0]).unwrap();
    // 5 is taken down: its edges are hidden, and it isn't unknown
    g.add(&uri(&d[5], FOLLOW, &tid(7, 0)), &d[0]).unwrap();
    g.add(&uri(&d[0], FOLLOW, &tid(8, 0)), &d[5]).unwrap();
    g.set_status(&d[5], Some(Status::Takendown), 9).unwrap();
    g.delete(&gone).unwrap();
    g.write_snapshot(&snap).unwrap();

    let extra = free_port();
    let cfg = Config {
        read_only: true,
        api_read_only: true,
        api_listen_extra: Some(extra.clone()),
        edges_among_max: 200,
        ..Config::new(snap)
    };
    let app = App::open(cfg).unwrap();
    let ui = Arc::new(marco_server::ui::Ui::load(None).unwrap());
    let (api, admin) = marco_server::serve(app.clone(), ui).await.unwrap();
    let extra: SocketAddr = extra.parse().unwrap();

    let unknown = "did:plc:unknownunknownunknownxx".to_string();
    let set = vec![d[0].clone(), d[1].clone(), d[2].clone(), unknown.clone(), d[0].clone(), d[5].clone()];
    let t = |a: usize, rel: &str, b: usize| (d[a].clone(), rel.to_string(), d[b].clone());
    for addr in [api, extra, admin] {
        let g = graph(addr);
        let r = g.edges_among(req(&set, &[], 0)).await.unwrap().into_owned();
        assert_eq!(
            triples(&r),
            BTreeSet::from([t(0, "follow", 1), t(1, "follow", 0), t(0, "follow", 2), t(2, "block", 0)])
        );
        assert_eq!(r.edges.iter().find(|e| e.to == d[1]).unwrap().uri, f01);
        assert!(r.edges.iter().all(|e| e.uri.starts_with(&format!("at://{}/", e.from))));
        assert_eq!(
            r.counts.iter().map(|c| (c.rel.as_str(), c.edges)).collect::<Vec<_>>(),
            [("follow", 3), ("block", 1)]
        );
        assert_eq!(r.unknown, std::slice::from_ref(&unknown));
        assert!(!r.truncated);

        let one = g.edges_among(req(&set, &["block", "follow"], 1)).await.unwrap().into_owned();
        assert_eq!(triples(&one), BTreeSet::from([t(2, "block", 0)]), "the first relation asked first");
        assert_eq!(one.counts.iter().map(|c| c.edges).collect::<Vec<_>>(), [1, 3]);
        assert!(one.truncated);

        let none = g.edges_among(req(&[d[1].clone(), d[4].clone()], &[], 0)).await.unwrap().into_owned();
        assert!(none.edges.is_empty() && none.unknown.is_empty() && !none.truncated);

        for rels in [["listitem"], ["listitem.owner"], ["listblock"], ["nope"]] {
            let e = g.edges_among(req(&set, &rels, 0)).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{rels:?}");
        }
        // past EDGES_AMONG_ON_WORKER, on the miss pool
        let mut big = set.clone();
        big.extend((100..400).map(did));
        let r = g.edges_among(req(&big[..200], &[], 0)).await.unwrap().into_owned();
        assert_eq!(triples(&r).len(), 4);
        assert_eq!(r.unknown.len(), 200 - 5);
        let e = g.edges_among(req(&big[..201], &[], 0)).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "past --edges-among-max-actors");
        assert!(e.message.unwrap_or_default().contains("at most 200 actors"));
    }

    // curl's JSON, as the docs show it
    let body = format!(r#"{{"actors":["{}","{}"],"rels":["follow"]}}"#, d[0], d[1]);
    let r = reqwest::Client::new()
        .post(format!("http://{extra}/marco.v1.Graph/EdgesAmong"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["counts"][0]["edges"], "2", "{v}");
    assert_eq!(v["edges"].as_array().unwrap().len(), 2, "{v}");
}
