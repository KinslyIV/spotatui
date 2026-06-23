use super::Network;
use crate::core::app::{Announcement, AnnouncementLevel, LyricsStatus};
use chrono::{DateTime, Utc};
use serde::{de::Error as _, Deserialize, Deserializer};
use std::collections::HashSet;
use std::env;
use std::time::{Duration, Instant};

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct LrcResponse {
  syncedLyrics: Option<String>,
  plainLyrics: Option<String>,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct GlobalSongCountResponse {
  #[serde(deserialize_with = "deserialize_global_song_count")]
  count: u64,
}

const TELEMETRY_ENDPOINT: &str = "https://spotatui-counter.spotatui.workers.dev";

fn deserialize_global_song_count<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
  D: Deserializer<'de>,
{
  #[derive(Deserialize)]
  #[serde(untagged)]
  enum CountValue {
    Number(u64),
    String(String),
  }

  match CountValue::deserialize(deserializer)? {
    CountValue::Number(value) => Ok(value),
    CountValue::String(value) => {
      let sanitized = value.replace(',', "");
      sanitized
        .parse::<u64>()
        .map_err(|_| D::Error::custom("invalid global song count"))
    }
  }
}

#[derive(Deserialize, Debug)]
struct AnnouncementFeedResponse {
  #[allow(dead_code)]
  version: Option<u8>,
  #[serde(default)]
  announcements: Vec<AnnouncementRecord>,
}

#[derive(Deserialize, Debug)]
struct AnnouncementRecord {
  id: String,
  title: Option<String>,
  body: String,
  level: Option<String>,
  url: Option<String>,
  starts_at: Option<String>,
  ends_at: Option<String>,
}

pub trait UtilsNetwork {
  async fn get_lyrics(&mut self, track: String, artist: String, duration: f64);
  async fn increment_global_song_count(&mut self);
  async fn fetch_global_song_count(&mut self);
  async fn fetch_announcements(&mut self);
}

impl UtilsNetwork for Network {
  async fn get_lyrics(&mut self, track: String, artist: String, duration: f64) {
    let client = super::requests::shared_http_client();
    let query = vec![
      ("track_name", track.clone()),
      ("artist_name", artist.clone()),
      ("duration", duration.to_string()),
    ];

    // Update state to loading
    {
      let mut app = self.app.lock().await;
      app.lyrics_status = LyricsStatus::Loading;
      app.lyrics = None;
    }

    match client
      .get("https://lrclib.net/api/get")
      .query(&query)
      .send()
      .await
    {
      Ok(resp) => {
        if resp.status().is_success() {
          if let Ok(lrc_resp) = resp.json::<LrcResponse>().await {
            let lyrics_text = lrc_resp
              .syncedLyrics
              .or(lrc_resp.plainLyrics)
              .unwrap_or_default();

            if !lyrics_text.is_empty() {
              let mut app = self.app.lock().await;
              // Simple LRC parser
              let parsed: Vec<(u128, String)> = lyrics_text
                .lines()
                .filter_map(|line| {
                  // [mm:ss.xx] text
                  if let Some(idx) = line.find(']') {
                    if idx > 1 && line.starts_with('[') {
                      let timestamp = &line[1..idx];
                      let content = line[idx + 1..].trim().to_string();

                      // Parse timestamp
                      let parts: Vec<&str> = timestamp.split(':').collect();
                      if parts.len() == 2 {
                        let mins = parts[0].parse::<u64>().unwrap_or(0);
                        let secs_parts: Vec<&str> = parts[1].split('.').collect();
                        let secs = secs_parts[0].parse::<u64>().unwrap_or(0);
                        let ms = if secs_parts.len() > 1 {
                          // Handle 2 or 3 digit ms
                          let ms_str = secs_parts[1];
                          let ms_val = ms_str.parse::<u64>().unwrap_or(0);
                          if ms_str.len() == 2 {
                            ms_val * 10
                          } else {
                            ms_val
                          }
                        } else {
                          0
                        };

                        let total_ms = (mins * 60 * 1000) + (secs * 1000) + ms;
                        return Some((total_ms as u128, content));
                      }
                    }
                  }
                  None
                })
                .collect();

              if !parsed.is_empty() {
                app.lyrics = Some(parsed);
                app.lyrics_status = LyricsStatus::Found;
              } else {
                app.lyrics_status = LyricsStatus::NotFound;
              }
            } else {
              let mut app = self.app.lock().await;
              app.lyrics_status = LyricsStatus::NotFound;
            }
          }
        } else {
          let mut app = self.app.lock().await;
          app.lyrics_status = LyricsStatus::NotFound;
        }
      }
      Err(_) => {
        let mut app = self.app.lock().await;
        app.lyrics_status = LyricsStatus::NotFound;
      }
    }
  }

  async fn increment_global_song_count(&mut self) {
    let client = super::requests::shared_http_client();
    // Fire and forget
    let _ = client
      .post(TELEMETRY_ENDPOINT)
      .header(reqwest::header::ACCEPT, "application/json")
      .timeout(Duration::from_secs(5))
      .send()
      .await;
  }

  async fn fetch_global_song_count(&mut self) {
    let client = super::requests::shared_http_client();
    match client
      .get(TELEMETRY_ENDPOINT)
      .header(reqwest::header::ACCEPT, "application/json")
      .timeout(Duration::from_secs(5))
      .send()
      .await
    {
      Ok(resp) => {
        if let Ok(data) = resp.json::<GlobalSongCountResponse>().await {
          let mut app = self.app.lock().await;
          app.global_song_count = Some(data.count);
          app.global_song_count_failed = false;
        } else {
          let mut app = self.app.lock().await;
          app.global_song_count_failed = true;
        }
      }
      Err(_) => {
        let mut app = self.app.lock().await;
        app.global_song_count_failed = true;
      }
    }
  }

  async fn fetch_announcements(&mut self) {
    const MAX_ANNOUNCEMENT_FEED_BYTES: usize = 256 * 1024;
    const ANNOUNCEMENTS_ENV_KEY: &str = "SPOTATUI_ANNOUNCEMENTS_URL";
    const DEFAULT_ANNOUNCEMENTS_URL: &str =
      "https://raw.githubusercontent.com/LargeModGames/spotatui/main/announcements.json";

    let (announcements_enabled, feed_url, seen_ids) = {
      let app = self.app.lock().await;
      (
        app.user_config.behavior.enable_announcements,
        app.user_config.behavior.announcement_feed_url.clone(),
        app.user_config.behavior.seen_announcement_ids.clone(),
      )
    };

    if !announcements_enabled {
      return;
    }

    let env_feed_url = env::var(ANNOUNCEMENTS_ENV_KEY)
      .ok()
      .map(|v| v.trim().to_string())
      .filter(|v| !v.is_empty());

    let resolved_url = env_feed_url
      .or(feed_url)
      .filter(|url| !url.trim().is_empty())
      .unwrap_or_else(|| DEFAULT_ANNOUNCEMENTS_URL.to_string());

    if !resolved_url.starts_with("https://") {
      return;
    }

    let client = match reqwest::Client::builder()
      .timeout(Duration::from_secs(5))
      .build()
    {
      Ok(client) => client,
      Err(_) => return,
    };

    let response = match client
      .get(&resolved_url)
      .header(reqwest::header::ACCEPT, "application/json")
      .send()
      .await
    {
      Ok(response) => response,
      Err(_) => return,
    };

    if !response.status().is_success() {
      return;
    }

    if response
      .content_length()
      .is_some_and(|length| length > MAX_ANNOUNCEMENT_FEED_BYTES as u64)
    {
      return;
    }

    let body = match response.bytes().await {
      Ok(bytes) if bytes.len() <= MAX_ANNOUNCEMENT_FEED_BYTES => bytes,
      _ => return,
    };

    let feed: AnnouncementFeedResponse = match serde_json::from_slice(&body) {
      Ok(feed) => feed,
      Err(_) => return,
    };

    let now = Utc::now();
    let seen_ids = seen_ids.into_iter().collect::<HashSet<String>>();
    let mut feed_ids_seen = HashSet::new();
    let mut announcements = Vec::new();

    for record in feed.announcements {
      let id = record.id.trim().to_string();
      if id.is_empty() || seen_ids.contains(&id) || !feed_ids_seen.insert(id.clone()) {
        continue;
      }

      let body = record.body.trim().to_string();
      if body.is_empty() {
        continue;
      }

      let starts_at = match record.starts_at.as_deref().map(parse_announcement_datetime) {
        Some(Some(value)) => Some(value),
        Some(None) => continue,
        None => None,
      };

      let ends_at = match record.ends_at.as_deref().map(parse_announcement_datetime) {
        Some(Some(value)) => Some(value),
        Some(None) => continue,
        None => None,
      };

      if let Some(start) = starts_at {
        if now < start {
          continue;
        }
      }

      if let Some(end) = ends_at {
        if now > end {
          continue;
        }
      }

      let url = record
        .url
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty() && url.starts_with("https://"));

      announcements.push(Announcement {
        id,
        title: record
          .title
          .map(|title| title.trim().to_string())
          .filter(|title| !title.is_empty())
          .unwrap_or_else(|| "Announcement".to_string()),
        body,
        level: parse_announcement_level(record.level.as_deref()),
        url,
        received_at: Instant::now(),
      });
    }

    if announcements.is_empty() {
      return;
    }

    let mut app = self.app.lock().await;
    let had_active_announcement = app.active_announcement.is_some();
    app.enqueue_announcements(announcements);

    if !had_active_announcement && app.active_announcement.is_some() {
      app.push_navigation_stack(
        crate::core::app::RouteId::AnnouncementPrompt,
        crate::core::app::ActiveBlock::AnnouncementPrompt,
      );
    }
  }
}

fn parse_announcement_datetime(value: &str) -> Option<DateTime<Utc>> {
  DateTime::parse_from_rfc3339(value)
    .ok()
    .map(|dt| dt.with_timezone(&Utc))
}

fn parse_announcement_level(level: Option<&str>) -> AnnouncementLevel {
  match level.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
    Some("critical") => AnnouncementLevel::Critical,
    Some("warning") => AnnouncementLevel::Warning,
    _ => AnnouncementLevel::Info,
  }
}
