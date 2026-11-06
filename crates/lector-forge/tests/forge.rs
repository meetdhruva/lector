//! GitHub and GitLab adapters against a local mock server with recorded JSON
//! (no real network).

use std::path::PathBuf;

use lector_forge::mock::{MockServer, Route};
use lector_forge::{
    connect, detect_with, ChecksState, DraftComment, Forge, ForgeError, ForgeHosts, PrFilter,
    PrState, ReviewEvent, ReviewSubmission, Side, Token,
};
use serde_json::Value;

fn fixtures(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn github(server: &MockServer) -> Box<dyn Forge> {
    let hosts = ForgeHosts {
        api_urls: [("github.com".to_string(), server.url())]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let remote = detect_with("git@github.com:acme/widgets.git", &hosts).expect("remote");
    connect(&remote, Token::new("ghp_secret"))
}

fn gitlab(server: &MockServer) -> Box<dyn Forge> {
    let hosts = ForgeHosts {
        gitlab: vec!["gitlab.corp".into()],
        api_urls: [(
            "gitlab.corp".to_string(),
            format!("{}/api/v4", server.url()),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let remote = detect_with("https://gitlab.corp/group/sub/app.git", &hosts).expect("remote");
    connect(&remote, Token::new("glpat_secret"))
}

#[test]
fn github_lists_and_filters() {
    let server = MockServer::from_dir(&fixtures("github"), 0).expect("server");
    let gh = github(&server);
    let all = gh.list_pull_requests(PrFilter::All).expect("list");
    assert_eq!(
        all.iter().map(|p| p.number).collect::<Vec<_>>(),
        vec![15, 12, 9],
        "newest update first"
    );
    assert!(all[0].draft);
    let mine = gh.list_pull_requests(PrFilter::Mine).expect("mine");
    assert_eq!(mine.iter().map(|p| p.number).collect::<Vec<_>>(), vec![15]);
    let review = gh
        .list_pull_requests(PrFilter::ReviewRequested)
        .expect("review");
    assert_eq!(
        review.iter().map(|p| p.number).collect::<Vec<_>>(),
        vec![12]
    );
    let reqs = server.requests();
    assert!(reqs
        .iter()
        .all(|r| r.header("authorization") == Some("Bearer ghp_secret")));
    assert_eq!(
        reqs[0].header("accept"),
        Some("application/vnd.github+json")
    );
}

#[test]
fn github_details_threads_and_review() {
    let server = MockServer::from_dir(&fixtures("github"), 0).expect("server");
    let gh = github(&server);
    let pr = gh.pull_request(12).expect("pr");
    assert_eq!(pr.head_sha, "1".repeat(40));
    assert_eq!(pr.base_ref, "main");
    assert_eq!(pr.state, PrState::Open);
    assert_eq!(pr.files.len(), 2);
    assert_eq!(pr.files[1].old_path.as_deref(), Some("src/load.rs"));
    assert_eq!(pr.checks, Some(ChecksState::Success));
    assert_eq!(pr.reviewers, vec!["ada".to_string()]);
    assert_eq!(
        gh.fetch_refspecs(&pr),
        vec![
            "+refs/pull/12/head:refs/lector/pr/12/head".to_string(),
            "+refs/heads/main:refs/lector/pr/12/base".to_string()
        ]
    );

    let threads = gh.review_threads(12).expect("threads");
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].path, "src/manifest.rs");
    assert_eq!(threads[0].line, Some(13), "0-based");
    assert_eq!(threads[0].side, Side::New);
    assert_eq!(threads[0].comments.len(), 2);
    assert_eq!(threads[0].comments[0].author, "linus");
    assert_eq!(threads[0].comments[0].id, "501");
    assert!(threads[1].resolved && threads[1].outdated);
    assert_eq!(threads[1].line, None);
    assert_eq!(threads[1].original_line, Some(2));
    assert_eq!(threads[1].start_line, Some(0));
    assert_eq!(threads[1].side, Side::Old);

    let reply = gh.reply(12, "PRRT_1", "Thanks!").expect("reply");
    assert_eq!(reply.id, "504");
    gh.resolve_thread(12, "PRRT_1", true).expect("resolve");

    let sub = gh
        .submit_review(
            12,
            &ReviewSubmission {
                event: ReviewEvent::Approve,
                body: "LGTM".into(),
                head_sha: pr.head_sha.clone(),
                comments: vec![
                    DraftComment {
                        path: "src/manifest.rs".into(),
                        line: 20,
                        start_line: Some(18),
                        side: Side::New,
                        body: "nit".into(),
                        ..Default::default()
                    },
                    DraftComment {
                        path: "src/loader.rs".into(),
                        line: 0,
                        side: Side::Old,
                        body: "gone".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        )
        .expect("submit");
    assert_eq!(sub.id, "80");
    let post = server
        .requests()
        .into_iter()
        .find(|r| r.target.ends_with("/reviews"))
        .expect("review request");
    let body: Value = serde_json::from_str(&post.body).expect("json");
    assert_eq!(body["event"], "APPROVE");
    assert_eq!(body["commit_id"], "1".repeat(40));
    assert_eq!(body["comments"][0]["line"], 21);
    assert_eq!(body["comments"][0]["start_line"], 19);
    assert_eq!(body["comments"][0]["side"], "RIGHT");
    assert_eq!(body["comments"][1]["side"], "LEFT");
    assert_eq!(body["comments"][1]["line"], 1);
    let gql = server
        .requests()
        .into_iter()
        .find(|r| r.body.contains("resolveReviewThread"))
        .expect("graphql");
    assert_eq!(gql.target, "/graphql");
}

#[test]
fn github_errors_are_typed() {
    let server = MockServer::start(
        0,
        vec![
            Route::json("GET", "/user", r#"{"message":"Bad credentials"}"#).status(401),
            Route::json(
                "GET",
                "/repos/acme/widgets/pulls",
                r#"{"message":"API rate limit exceeded for user"}"#,
            )
            .status(403)
            .header("x-ratelimit-remaining", "0")
            .header("x-ratelimit-reset", "4102444800"),
            Route::json(
                "GET",
                "/repos/acme/widgets/pulls/99",
                r#"{"message":"Not Found"}"#,
            )
            .status(404),
            Route::json(
                "POST",
                "/graphql",
                r#"{"errors":[{"message":"Resource not accessible by integration"}]}"#,
            ),
            Route::json("GET", "/repos/acme/widgets/pulls/5", "<html>oops").status(502),
        ],
    )
    .expect("server");
    let gh = github(&server);
    match gh.viewer() {
        Err(ForgeError::Unauthorized { message, help, .. }) => {
            assert_eq!(message, "Bad credentials");
            assert!(help.contains("GITHUB_TOKEN"));
        }
        other => panic!("{other:?}"),
    }
    match gh.list_pull_requests(PrFilter::All) {
        Err(e @ ForgeError::RateLimited { reset: Some(_), .. }) => {
            assert!(e.to_string().contains("rate limit"))
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(gh.pull_request(99), Err(ForgeError::NotFound(_))));
    assert!(matches!(
        gh.review_threads(1),
        Err(ForgeError::Http { status: 200, .. })
    ));
    match gh.pull_request(5) {
        Err(ForgeError::Http {
            status: 502,
            message,
        }) => assert!(message.contains("oops")),
        other => panic!("{other:?}"),
    }
    // Error messages never contain the token.
    for r in server.requests() {
        assert!(!r.target.contains("ghp_secret"));
    }
}

#[test]
fn github_follows_pagination() {
    let server = MockServer::start(0, vec![]).expect("server");
    let page = |n: u64| {
        format!(
            r#"[{{"number": {n}, "title": "t{n}", "state": "open", "user": {{"login": "x"}}, "head": {{"sha": "a", "ref": "h"}}, "base": {{"sha": "b", "ref": "main"}}}}]"#
        )
    };
    server.add(
        Route::json("GET", "/repos/acme/widgets/pulls", page(1)).header(
            "link",
            &format!(
                r#"<{}/repos/acme/widgets/pulls?page=2>; rel="next""#,
                server.url()
            ),
        ),
    );
    server.add(Route::json(
        "GET",
        "/repos/acme/widgets/pulls?page=2",
        page(2),
    ));
    let gh = github(&server);
    let all = gh.list_pull_requests(PrFilter::All).expect("list");
    let mut n: Vec<u64> = all.iter().map(|p| p.number).collect();
    n.sort();
    assert_eq!(n, vec![1, 2]);
}

#[test]
fn gitlab_merge_requests_discussions_and_review() {
    let server = MockServer::from_dir(&fixtures("gitlab"), 0).expect("server");
    let gl = gitlab(&server);
    let list = gl
        .list_pull_requests(PrFilter::ReviewRequested)
        .expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].comments, 4);
    let q = server
        .requests()
        .into_iter()
        .find(|r| r.target.contains("/merge_requests?"))
        .expect("list request");
    assert!(q.target.contains("reviewer_username=ada"), "{}", q.target);
    assert_eq!(q.header("authorization"), Some("Bearer glpat_secret"));

    let mr = gl.pull_request(3).expect("mr");
    assert_eq!(mr.base_sha, "b".repeat(4) + &"0".repeat(36));
    assert_eq!(mr.head_ref, "token-cache");
    assert_eq!(mr.checks, Some(ChecksState::Pending));
    assert_eq!(mr.files.len(), 2);
    assert_eq!((mr.files[0].additions, mr.files[0].deletions), (2, 1));
    assert_eq!(mr.files[1].status, "added");
    assert_eq!(
        gl.fetch_refspecs(&mr)[0],
        "+refs/merge-requests/3/head:refs/lector/pr/3/head"
    );

    let threads = gl.review_threads(3).expect("threads");
    assert_eq!(threads.len(), 2, "general and system notes are skipped");
    assert_eq!(threads[0].id, "d1");
    assert_eq!(threads[0].line, Some(1));
    assert_eq!(threads[0].side, Side::New);
    assert_eq!(threads[0].comments.len(), 2);
    assert!(threads[1].resolved);
    assert_eq!(threads[1].side, Side::Old);

    gl.reply(3, "d1", "ok").expect("reply");
    gl.resolve_thread(3, "d1", true).expect("resolve");
    let sub = gl
        .submit_review(
            3,
            &ReviewSubmission {
                event: ReviewEvent::Approve,
                body: "Nice".into(),
                head_sha: mr.head_sha.clone(),
                base_sha: mr.base_sha.clone(),
                start_sha: "c".repeat(4) + &"0".repeat(36),
                comments: vec![DraftComment {
                    path: "src/auth.rs".into(),
                    line: 2,
                    side: Side::New,
                    body: "nit".into(),
                    ..Default::default()
                }],
            },
        )
        .expect("submit");
    assert_eq!(sub.threads, vec![Some("d-new".to_string())]);
    let reqs = server.requests();
    let disc = reqs
        .iter()
        .find(|r| r.method == "POST" && r.target.ends_with("/discussions"))
        .expect("discussion");
    let body: Value = serde_json::from_str(&disc.body).expect("json");
    assert_eq!(body["position"]["new_line"], 3);
    assert_eq!(body["position"]["position_type"], "text");
    assert!(reqs.iter().any(|r| r.target.ends_with("/approve")));
    assert!(reqs
        .iter()
        .any(|r| r.method == "PUT" && r.target.contains("resolved=true")));

    let rc = gl
        .submit_review(
            3,
            &ReviewSubmission {
                event: ReviewEvent::RequestChanges,
                ..Default::default()
            },
        )
        .expect("request changes");
    assert_eq!(rc.notes.len(), 1);
}
