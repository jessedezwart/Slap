use super::*;
use crate::{config::SpotifyConfig, media::PLAYLIST_LIMIT as LIMIT};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    let mut service = SpotifyService::new(Some(SpotifyConfig {
        client_id: "client".into(),
        client_secret: "secret".into(),
        refresh_token: refresh_token.map(str::to_owned),
    }))
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
        (200, json!({"name":"Late Night"})),
    ])
    .await;
    let list = service(&base, Some("refresh"))
        .playlist_by_id("playlist")
        .await
        .unwrap();
    assert_eq!(list.tracks.len(), 2);
    assert_eq!(list.skipped, 1);
    assert!(!list.truncated);
    assert_eq!(
        list.tracks[1].metadata.title.as_deref(),
        Some("Artist, Guest — Last")
    );
    assert_eq!(list.playlist_name.as_deref(), Some("Late Night"));

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
    // The name is a label, so it is only looked up once the tracks are in.
    assert!(requests[5].starts_with("GET /playlists/playlist?fields=name "));
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
    responses.push((200, json!({"name":"Repeats"})));
    let (base, task) = server(responses).await;
    let list = service(&base, Some("refresh"))
        .playlist_by_id("playlist")
        .await
        .unwrap();
    assert_eq!(list.tracks.len(), LIMIT);
    assert!(list.truncated);
    assert_eq!(task.await.unwrap().len(), pages + 2);
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
    assert_eq!(list.playlist_name.as_deref(), Some("LEVELING"));
    // `total` exceeds what was returned, so the user is told it was cut short.
    assert!(list.truncated);

    let requests = task.await.unwrap();
    assert!(requests[0].contains("grant_type=client_credentials"));
    assert!(!requests[0].contains("refresh_token"));
    // The playlist object, not the items endpoint that would 401.
    assert!(requests[1].starts_with("GET /playlists/playlist "));
}

#[tokio::test]
async fn client_credentials_reports_a_playlist_it_cannot_list() {
    let (base, task) = server(vec![
        (200, json!({"access_token":"app"})),
        (200, json!({"name":"Jazz House Vibes", "owner":{"id":"someone"}})),
    ])
    .await;
    assert!(matches!(
        service(&base, None).playlist_by_id("playlist").await,
        Err(SpotifyError::PlaylistNotOwned)
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn actionable_errors_and_empty_playlist() {
    let disabled = SpotifyService::new(None).unwrap();
    assert!(
        disabled
            .playlist_by_id("unused")
            .await
            .unwrap_err()
            .to_string()
            .contains("SPOTIFY_CLIENT_ID")
    );
    for (status, expected) in [
        (403, "bot owner's account"),
        (404, "private"),
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

#[tokio::test]
async fn imports_a_single_track_and_reports_a_missing_one() {
    let (base, task) = server(vec![
        (200, json!({"access_token":"token"})),
        (200, song("Single")["item"].clone()),
        (404, json!({})),
    ])
    .await;
    let spotify = service(&base, None);
    let single = spotify.track_by_id("2I67vuiIAcmcchh7037NDP").await.unwrap();
    assert_eq!(single.tracks.len(), 1);
    assert_eq!(single.skipped, 0);
    assert!(single.playlist_name.is_none());
    assert!(matches!(
        spotify.track_by_id("missing").await,
        Err(SpotifyError::TrackNotFound)
    ));
    let requests = task.await.unwrap();
    assert!(requests[1].starts_with("GET /tracks/2I67vuiIAcmcchh7037NDP "));
}

#[tokio::test]
async fn imports_an_albums_first_page_with_its_cover_and_name() {
    let item = |name| song(name)["item"].clone();
    let (base, task) = server(vec![
        (200, json!({"access_token":"token"})),
        (
            200,
            json!({"name":"Debut", "images":[{"url":"https://i.scdn.co/image/cover"}],
                "tracks":{"items":[item("One"), item("Two")], "total":60, "next":"https://untrusted.invalid/never-follow"}}),
        ),
        (404, json!({})),
    ])
    .await;
    let spotify = service(&base, None);
    let album = spotify.album_by_id("album").await.unwrap();
    assert_eq!(album.playlist_name.as_deref(), Some("Debut"));
    assert_eq!(album.tracks.len(), 2);
    assert!(album.truncated);
    assert!(album.tracks.iter().all(
        |t| t.metadata.thumbnail.as_deref() == Some("https://i.scdn.co/image/cover")
    ));
    assert!(matches!(
        spotify.album_by_id("missing").await,
        Err(SpotifyError::AlbumNotFound)
    ));
    let requests = task.await.unwrap();
    assert!(requests[1].starts_with("GET /albums/album "));
}
