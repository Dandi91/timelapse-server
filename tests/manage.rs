//! Managing streams over HTTP: login, changes, live events.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures_util::StreamExt;
use reqwest::{Client, Method, StatusCode, redirect};
use serde_json::{Value, json};
use timelapse_server::server;
use timelapse_server::web::auth;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Api {
    http: Client,
    base: String,
    cookie: Option<String>,
}

impl Api {
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    async fn json(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let response = self.send(method, path, body).await;
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }
}

/// Collect server-sent events into a channel.
async fn subscribe(api: &Api) -> mpsc::UnboundedReceiver<Value> {
    let response = api.send(Method::GET, "/api/events", None).await;
    assert_eq!(response.status(), 200);
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut body = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(Ok(chunk)) = body.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..end + 2).collect();
                for data in frame.lines().filter_map(|l| l.strip_prefix("data: ")) {
                    if let Ok(value) = serde_json::from_str(data) {
                        let _ = tx.send(value);
                    }
                }
            }
        }
    });
    rx
}

/// Wait for an event matching `want`, skipping others.
async fn expect_event(
    events: &mut mpsc::UnboundedReceiver<Value>,
    within: Duration,
    want: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(within, async {
        loop {
            let event = events.recv().await.expect("event stream ended");
            if want(&event) {
                return event;
            }
        }
    })
    .await
    .expect("expected event did not arrive")
}

#[tokio::test]
async fn manages_streams_over_http_behind_a_login() {
    // The fetcher fails at once, so the recorder cycles through statuses without recording.
    let (mut ctx, _dir) = fixture_ctx("echo 'ERROR: nope' >&2\nexit 1").await;
    ctx.password_hash = Some(auth::hash("hunter2"));
    // Only the wake-up from the API can start a recorder quickly now.
    ctx.tuning.poll_interval = Duration::from_secs(60);
    let ctx = Arc::new(ctx);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(server::serve(ctx.clone(), shutdown.clone(), Some(listener)));

    let http = Client::builder().redirect(redirect::Policy::none()).build().unwrap();
    let mut api = Api {
        http,
        base,
        cookie: None,
    };

    // Locked out without a session; the login page itself is reachable.
    assert_eq!(api.json(Method::GET, "/api/streams", None).await.0, 401);
    assert_eq!(
        api.send(Method::GET, "/streams/1/playlist.m3u8", None).await.status(),
        401
    );
    let page = api.send(Method::GET, "/", None).await;
    assert!(page.status().is_redirection());
    assert_eq!(page.headers()["location"], "/login.html");
    assert_eq!(api.send(Method::GET, "/login.html", None).await.status(), 200);
    let (_, auth_state) = api.json(Method::GET, "/api/auth", None).await;
    assert_eq!(auth_state, json!({"required": true, "logged_in": false}));

    let wrong = api
        .send(Method::POST, "/api/login", Some(json!({"password": "nope"})))
        .await;
    assert_eq!(wrong.status(), 401);
    let right = api
        .send(Method::POST, "/api/login", Some(json!({"password": "hunter2"})))
        .await;
    assert_eq!(right.status(), 204);
    let set_cookie = right.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    api.cookie = Some(set_cookie.split(';').next().unwrap().to_string());
    assert_eq!(api.json(Method::GET, "/api/auth", None).await.1["logged_in"], true);

    let mut events = subscribe(&api).await;

    // Create: the recorder starts right away, not at the next poll a minute later.
    let (status, created) = api
        .json(
            Method::POST,
            "/api/streams",
            Some(json!({"label": "cam", "url": "https://example.com/cam", "settings": {"height": 180}})),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["settings"]["height"], 180);
    assert_eq!(
        created["settings"]["sample_fps"], 5.0,
        "unspecified settings take defaults"
    );
    let id = created["id"].as_i64().unwrap();
    expect_event(&mut events, Duration::from_secs(3), |e| e["type"] == "streams_changed").await;
    expect_event(&mut events, Duration::from_secs(3), |e| {
        e["type"] == "status" && e["status"] == "recording"
    })
    .await;
    let retrying = expect_event(&mut events, Duration::from_secs(5), |e| e["status"] == "retrying").await;
    assert_eq!(retrying["detail"], "ERROR: nope");
    assert_eq!(retrying["stream_id"], id);

    // Bad input comes back as a message for the user.
    let (status, error) = api
        .json(
            Method::POST,
            "/api/streams",
            Some(json!({"label": "x", "url": "https://e.com", "settings": {"sample_fps": 7}})),
        )
        .await;
    assert_eq!(status, 400);
    assert!(error["error"].as_str().unwrap().contains("sample-fps"), "{error}");
    let (status, error) = api
        .json(
            Method::POST,
            "/api/streams",
            Some(json!({"label": "cam", "url": "https://example.com/2"})),
        )
        .await;
    assert_eq!(
        (status, error["error"].as_str().unwrap()),
        (StatusCode::CONFLICT, "another stream already has that label")
    );
    assert_eq!(
        api.json(Method::POST, "/api/streams", Some(json!({"label": "y"})))
            .await
            .0,
        400
    );

    // Update merges settings and sets or clears limits.
    let path = format!("/api/streams/{id}");
    let (status, updated) = api
        .json(
            Method::PATCH,
            &path,
            Some(json!({"settings": {"crf": 30}, "max_bytes": 1000})),
        )
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(
        (
            updated["settings"]["crf"].clone(),
            updated["settings"]["height"].clone()
        ),
        (json!(30), json!(180))
    );
    assert_eq!(updated["max_bytes"], 1000);
    let (_, cleared) = api.json(Method::PATCH, &path, Some(json!({"max_bytes": null}))).await;
    assert_eq!(cleared["max_bytes"], Value::Null);
    assert_eq!(
        api.json(Method::PATCH, "/api/streams/999", Some(json!({"enabled": false})))
            .await
            .0,
        404
    );

    assert_eq!(
        api.send(Method::POST, &format!("{path}/restart"), None).await.status(),
        202
    );

    let log = api.send(Method::GET, &format!("{path}/log?lines=50"), None).await;
    assert_eq!(log.status(), 200);
    assert!(log.text().await.unwrap().contains("ERROR: nope"));

    let (status, system) = api.json(Method::GET, "/api/system", None).await;
    assert_eq!(status, 200);
    assert_eq!(system["recordings_bytes"], 0);
    assert!(system["disk_total_bytes"].as_u64().unwrap() >= system["disk_free_bytes"].as_u64().unwrap());

    // Delete: gone from the list, files removed once the recorder stops.
    assert_eq!(api.send(Method::DELETE, &path, None).await.status(), 204);
    expect_event(&mut events, Duration::from_secs(3), |e| e["type"] == "streams_changed").await;
    assert_eq!(api.json(Method::GET, "/api/streams", None).await.1, json!([]));
    let dir = ctx.stream_dir(id);
    assert!(
        wait_for(Duration::from_secs(10), || async { !dir.exists() }).await,
        "stream files left behind"
    );

    // Logging out ends the session.
    assert_eq!(api.send(Method::POST, "/api/logout", None).await.status(), 204);
    assert_eq!(api.json(Method::GET, "/api/streams", None).await.0, 401);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn without_a_password_everything_is_open() {
    let f = fixture("exit 1").await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(server::serve(f.ctx.clone(), shutdown.clone(), Some(listener)));
    let api = Api {
        http: Client::new(),
        base,
        cookie: None,
    };

    assert_eq!(api.json(Method::GET, "/api/streams", None).await.0, 200);
    assert_eq!(
        api.json(Method::GET, "/api/auth", None).await.1,
        json!({"required": false, "logged_in": true})
    );
    // An open event stream must not hold up shutdown.
    let _events = subscribe(&api).await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .expect("server did not stop")
        .unwrap()
        .unwrap();
}
