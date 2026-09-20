use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn song(name: &str) -> Value {
    json!({"item": {"type":"track", "name":name,
        "artists":[{"name":"Artist"},{"name":"Guest"}], "duration_ms":123456,
        "external_urls":{"spotify":"https://open.spotify.com/track/example"}}})
}

#[test]
fn playlist_links_and_unsupported_spotify_links() {
    for path in [
        "playlist/1234567890123456789012?si=abc",
        "intl-nl/playlist/1234567890123456789012/",
    ] {
        assert_eq!(
            playlist_id(&Url::parse(&format!("https://open.spotify.com/{path}")).unwrap()).unwrap(),
            "1234567890123456789012"
        );
    }
    for raw in [
        "https://spotify.link/abc",
        "https://open.spotify.com/album/1234567890123456789012",
        "https://open.spotify.com/playlist/invalid",
        "https://example.com/playlist/1234567890123456789012",
    ] {
        assert!(playlist_id(&Url::parse(raw).unwrap()).is_err());
    }
    assert!(!is_spotify_url(
        &Url::parse("https://open.spotify.com.evil.test/playlist/x").unwrap()
    ));
}

#[test]
fn metadata_becomes_artist_and_title_search() {
    let listed = listed_track(&song("Song & punctuation?!")).unwrap();
    let PlaybackTarget::Search(query) = &listed.target else {
        panic!("expected a search");
    };
    assert_eq!(query.artists, ["Artist", "Guest"]);
    assert_eq!(query.title, "Song & punctuation?!");
    assert_eq!(query.duration, Some(Duration::from_millis(123456)));
    assert_eq!(query.text(), "Artist, Guest Song & punctuation?!");
    assert_eq!(
        listed.metadata.duration,
        Some(Duration::from_millis(123456))
    );
    let mut old = song("Legacy");
    old["track"] = old.as_object_mut().unwrap().remove("item").unwrap();
    assert!(listed_track(&old).is_some());
    for entry in [
        Value::Null,
        json!({"item":null}),
        json!({"item":{"type":"episode"}}),
        json!({"item":{"type":"track", "name":"", "artists":[]}}),
    ] {
        assert!(listed_track(&entry).is_none());
    }
    let mut local = song("Local");
    local["is_local"] = json!(true);
    assert!(listed_track(&local).is_none());
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

fn client(base: &str) -> Spotify {
    let mut spotify = Spotify::new(Some(SpotifyConfig {
        client_id: "client".into(),
        client_secret: "secret".into(),
        refresh_token: "refresh".into(),
    }))
    .unwrap();
    spotify.api_base = base.into();
    spotify.token_url = format!("{base}/token");
    spotify
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
    let list = client(&base).list_id("playlist").await.unwrap();
    assert_eq!(list.tracks.len(), 2);
    assert_eq!(list.skipped, 1);
    assert!(!list.truncated);
    assert_eq!(
        list.tracks[1].metadata.title.as_deref(),
        Some("Artist, Guest — Last")
    );
    let requests = task.await.unwrap();
    assert!(requests[0].contains("grant_type=refresh_token&refresh_token=refresh"));
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
    let pages = PLAYLIST_LIMIT / 50;
    for _ in 0..pages {
        responses.push((
            200,
            json!({"items":vec![song("Repeated"); 50], "next":"more"}),
        ));
    }
    let (base, task) = server(responses).await;
    let list = client(&base).list_id("playlist").await.unwrap();
    assert_eq!(list.tracks.len(), PLAYLIST_LIMIT);
    assert!(list.truncated);
    assert_eq!(task.await.unwrap().len(), pages + 1);
}

#[tokio::test]
async fn actionable_errors_and_empty_playlist() {
    let disabled = Spotify::new(None).unwrap();
    assert!(
        disabled
            .list_id("unused")
            .await
            .unwrap_err()
            .to_string()
            .contains("SPOTIFY_CLIENT_ID")
    );
    for (status, expected) in [
        (403, "own or collaborate"),
        (404, "not found"),
        (429, "rate limit"),
    ] {
        let (base, task) = server(vec![
            (200, json!({"access_token":"token"})),
            (status, json!({})),
        ])
        .await;
        assert!(
            client(&base)
                .list_id("playlist")
                .await
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
        task.await.unwrap();
    }
    let (base, task) = server(vec![
        (200, json!({"access_token":"token"})),
        (200, json!({"items":[], "next":null})),
    ])
    .await;
    assert!(
        client(&base)
            .list_id("playlist")
            .await
            .unwrap()
            .tracks
            .is_empty()
    );
    task.await.unwrap();
}
