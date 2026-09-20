use super::*;
use crate::config::SpotifyConfig;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Small enough to exercise the limit in two pages.
const LIMIT: usize = 100;

fn song(name: &str) -> Value {
    json!({"item": {"type":"track", "id":"4cOdK2wGLETKBW3PvgPWqT", "name":name,
        "artists":[{"name":"Artist"},{"name":"Guest"}], "duration_ms":123456,
        "external_urls":{"spotify":"https://open.spotify.com/track/example"}}})
}

// A local HTTP server verifies real auth, pagination, limits and error handling.
async fn server(responses: Vec<(u16, Value)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")?.parse::<usize>().ok()
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(request).unwrap());
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (base, task)
}

fn service(base: &str, refresh_token: Option<&str>) -> SpotifyService {
    let mut service = SpotifyService::new(
        Some(SpotifyConfig {
            client_id: "client".into(),
            client_secret: "secret".into(),
            refresh_token: refresh_token.map(str::to_owned),
        }),
        LIMIT,
    )
    .unwrap();
    service.client.api_base = base.into();
    service.client.token_url = format!("{base}/token");
    service
}

#[tokio::test]
async fn imports_pages_in_order_skips_unavailable_and_refreshes_authorization() {
    let (base, task) = server(vec![
        (
            200,
            json!({"access_token":"first", "refresh_token":"rotated"}),
        ),
        (
            200,
            json!({"items":[song("First"), null], "next":"https://untrusted.invalid/never-follow"}),
        ),
        (401, json!({})),
        (200, json!({"access_token":"second"})),
        (200, json!({"items":[song("Last")], "next":null})),
    ])
    .await;
    let list = service(&base, Some("refresh"))
        .playlist_by_id("playlist")
        .await
        .unwrap();
    assert_eq!(list.tracks.len(), 2);
    assert_eq!(list.skipped, 1);
    assert!(!list.truncated);
    assert_eq!(list.tracks[1].title, "Last");

    let requests = task.await.unwrap();
    assert!(requests[0].contains("grant_type=refresh_token&refresh_token=refresh"));
    // The next page is requested by offset, never by following the response's
    // own `next` URL.
    assert!(requests[2].starts_with("GET /playlists/playlist/items?limit=50&offset=2 "));
    assert!(requests[3].contains("refresh_token=rotated"));
    assert!(
        requests[4]
            .to_ascii_lowercase()
            .contains("authorization: bearer second")
    );
}

#[tokio::test]
async fn import_stops_at_limit_and_reports_truncation() {
    let mut responses = vec![(200, json!({"access_token":"token"}))];
    let pages = LIMIT / 50;
    for _ in 0..pages {
        responses.push((
            200,
            json!({"items":vec![song("Repeated"); 50], "next":"more"}),
        ));
    }
    let (base, task) = server(responses).await;
    let list = service(&base, Some("refresh"))
        .playlist_by_id("playlist")
        .await
        .unwrap();
    assert_eq!(list.tracks.len(), LIMIT);
    assert!(list.truncated);
    assert_eq!(task.await.unwrap().len(), pages + 1);
}

// Spotify requires user auth for `/playlists/{id}/items`, so without a refresh
// token the only tracks reachable are the ones embedded in the playlist object.
#[tokio::test]
async fn client_credentials_reads_only_the_embedded_first_page() {
    let (base, task) = server(vec![
        (200, json!({"access_token":"app"})),
        (
            200,
            json!({"name":"LEVELING", "items":{
                "total": 551,
                "limit": 100,
                "next": "https://api.spotify.com/v1/playlists/playlist/items?offset=100",
                "items": [song("First"), song("Second")]}}),
        ),
    ])
    .await;
    let list = service(&base, None)
        .playlist_by_id("playlist")
        .await
        .unwrap();
    assert_eq!(list.tracks.len(), 2);
    // `total` exceeds what was returned, so the user is told it was cut short.
    assert!(list.truncated);

    let requests = task.await.unwrap();
    assert!(requests[0].contains("grant_type=client_credentials"));
    assert!(!requests[0].contains("refresh_token"));
    // The playlist object, not the items endpoint that would 401.
    assert!(requests[1].starts_with("GET /playlists/playlist "));
}

/// Confirms against the real API that client credentials reach a public
/// playlist at all, and that only a user token pages past the first 100.
#[tokio::test]
#[ignore = "requires Spotify credentials and network access"]
async fn live_public_playlist() {
    let url =
        Url::parse("https://open.spotify.com/playlist/6PWze6begwQa2HBTFz6TZS?si=4509417a06274f89")
            .unwrap();
    let config = |refresh_token| {
        Some(SpotifyConfig {
            client_id: std::env::var("SPOTIFY_CLIENT_ID").unwrap(),
            client_secret: std::env::var("SPOTIFY_CLIENT_SECRET").unwrap(),
            refresh_token,
        })
    };

    let app = SpotifyService::new(config(None), 1000)
        .unwrap()
        .playlist(&url)
        .await
        .unwrap();
    println!(
        "client credentials: {} tracks, {} skipped, truncated {}",
        app.tracks.len(),
        app.skipped,
        app.truncated
    );
    assert!(!app.tracks.is_empty(), "a public playlist must be readable");
    assert!(
        app.tracks.len() <= 100,
        "the items endpoint needs a user token"
    );

    let Ok(refresh) = std::env::var("SPOTIFY_REFRESH_TOKEN") else {
        return;
    };
    let user = SpotifyService::new(config(Some(refresh)), 1000)
        .unwrap()
        .playlist(&url)
        .await
        .unwrap();
    println!(
        "user token: {} tracks, {} skipped, truncated {}",
        user.tracks.len(),
        user.skipped,
        user.truncated
    );
    assert!(
        user.tracks.len() > app.tracks.len(),
        "a user token must page past the embedded first page"
    );
    let first = &user.tracks[0];
    println!(
        "first: {} — {} ({:?})",
        first.artist(),
        first.title,
        first.duration
    );
}

#[tokio::test]
async fn actionable_errors_and_empty_playlist() {
    let disabled = SpotifyService::new(None, LIMIT).unwrap();
    assert!(
        disabled
            .playlist_by_id("unused")
            .await
            .unwrap_err()
            .to_string()
            .contains("SPOTIFY_CLIENT_ID")
    );
    for (status, expected) in [
        (403, "set to public"),
        (404, "set to public"),
        (429, "rate limit"),
    ] {
        let (base, task) = server(vec![
            (200, json!({"access_token":"token"})),
            (status, json!({})),
        ])
        .await;
        assert!(
            service(&base, Some("refresh"))
                .playlist_by_id("playlist")
                .await
                .unwrap_err()
                .to_string()
                .contains(expected),
            "HTTP {status}"
        );
        task.await.unwrap();
    }
    let (base, task) = server(vec![
        (200, json!({"access_token":"token"})),
        (200, json!({"items":[], "next":null})),
    ])
    .await;
    assert!(
        service(&base, Some("refresh"))
            .playlist_by_id("playlist")
            .await
            .unwrap()
            .tracks
            .is_empty()
    );
    task.await.unwrap();
}
