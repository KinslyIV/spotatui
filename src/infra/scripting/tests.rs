use crate::core::plugin_api::{PlaybackState, TrackInfo};

use super::effects::ScriptEffect;
use super::engine::ScriptEngine;
use super::events::{diff_events, ScriptEvent};

fn track(uri: &str, name: &str) -> TrackInfo {
  TrackInfo {
    uri: Some(uri.to_string()),
    name: name.to_string(),
    artists: vec!["Artist".to_string()],
    album: "Album".to_string(),
    duration_ms: 200_000,
  }
}

fn playback(track: Option<TrackInfo>, is_playing: bool, progress_ms: u64) -> PlaybackState {
  PlaybackState {
    track,
    is_playing,
    progress_ms,
    shuffle: false,
    repeat: "off".to_string(),
    volume_percent: Some(50),
    device: None,
  }
}

/// Take all currently-queued effects out of the shared buffer.
/// (`ScriptEffect` is not `PartialEq` because `IoEvent` isn't, so tests pattern-match.)
fn drain(engine: &ScriptEngine) -> Vec<ScriptEffect> {
  engine.shared.effects.borrow_mut().drain(..).collect()
}

/// Assert a single effect was queued and return it.
fn one(engine: &ScriptEngine) -> ScriptEffect {
  let mut effects = drain(engine);
  assert_eq!(effects.len(), 1, "expected exactly one effect");
  effects.pop().unwrap()
}

// --- handler registration + emission ---

#[test]
fn track_change_handler_queues_notify() {
  let mut engine = ScriptEngine::new().unwrap();
  engine
    .load_source(
      "test",
      r#"
        spotatui.on("track_change", function(pb)
          spotatui.notify("now: " .. pb.track.name, 5)
        end)
      "#,
    )
    .unwrap();

  *engine.shared.playback.borrow_mut() = Some(playback(Some(track("uri:1", "Song A")), true, 0));
  engine.emit(ScriptEvent::TrackChange);

  match one(&engine) {
    ScriptEffect::Notify(msg, ttl) => {
      assert_eq!(msg, "now: Song A");
      assert_eq!(ttl, 5);
    }
    other => panic!("unexpected effect: {:?}", std::mem::discriminant(&other)),
  }
}

#[test]
fn erroring_handler_is_disabled_after_one_strike() {
  let mut engine = ScriptEngine::new().unwrap();
  engine
    .load_source(
      "bad",
      r#"
        spotatui.on("start", function() error("boom") end)
        spotatui.on("start", function() spotatui.notify("healthy", 1) end)
      "#,
    )
    .unwrap();

  engine.emit(ScriptEvent::Start);
  let first = drain(&engine);
  // One error notify (from the bad handler) plus the healthy notify.
  assert_eq!(first.len(), 2);
  match &first[0] {
    ScriptEffect::NotifyError(m, 6) => assert!(m.contains("error in on_start")),
    _ => panic!("expected error notify first"),
  }
  match &first[1] {
    ScriptEffect::Notify(m, 1) => assert_eq!(m, "healthy"),
    _ => panic!("expected healthy notify second"),
  }

  // Second emit: bad handler removed, only the healthy one fires (no new error).
  engine.emit(ScriptEvent::Start);
  match one(&engine) {
    ScriptEffect::Notify(m, 1) => assert_eq!(m, "healthy"),
    _ => panic!("expected only the healthy notify"),
  }
}

#[test]
fn unknown_event_name_is_an_error() {
  let mut engine = ScriptEngine::new().unwrap();
  let result = engine.load_source("test", r#"spotatui.on("bogus_event", function() end)"#);
  assert!(result.is_err());
}

// --- action functions queue the right effect ---

fn run_action(src: &str) -> ScriptEffect {
  let mut engine = ScriptEngine::new().unwrap();
  engine.load_source("test", src).unwrap();
  one(&engine)
}

#[test]
fn action_play_queues_play() {
  matches!(run_action("spotatui.play()"), ScriptEffect::Play);
}

#[test]
fn action_pause_queues_pause() {
  matches!(run_action("spotatui.pause()"), ScriptEffect::Pause);
}

#[test]
fn action_next_queues_next() {
  matches!(run_action("spotatui.next()"), ScriptEffect::Next);
}

#[test]
fn action_previous_queues_previous() {
  matches!(run_action("spotatui.previous()"), ScriptEffect::Previous);
}

#[test]
fn action_seek_queues_seek() {
  match run_action("spotatui.seek(12345)") {
    ScriptEffect::Seek(ms) => assert_eq!(ms, 12345),
    other => panic!("unexpected effect: {:?}", std::mem::discriminant(&other)),
  }
}

#[test]
fn action_set_volume_clamps_above_100() {
  match run_action("spotatui.set_volume(250)") {
    ScriptEffect::SetVolume(v) => assert_eq!(v, 100),
    other => panic!("unexpected effect: {:?}", std::mem::discriminant(&other)),
  }
  match run_action("spotatui.set_volume(-10)") {
    ScriptEffect::SetVolume(v) => assert_eq!(v, 0),
    other => panic!("unexpected effect: {:?}", std::mem::discriminant(&other)),
  }
}

#[test]
fn action_shuffle_queues_set_shuffle() {
  match run_action("spotatui.shuffle(true)") {
    ScriptEffect::SetShuffle(on) => assert!(on),
    other => panic!("unexpected effect: {:?}", std::mem::discriminant(&other)),
  }
  match run_action("spotatui.shuffle(false)") {
    ScriptEffect::SetShuffle(on) => assert!(!on),
    other => panic!("unexpected effect: {:?}", std::mem::discriminant(&other)),
  }
}

#[test]
fn action_search_queues_search_effect() {
  match run_action(r#"spotatui.search("daft punk")"#) {
    ScriptEffect::Search(q) => assert_eq!(q, "daft punk"),
    _ => panic!("expected a Search effect"),
  }
}

#[test]
fn action_notify_default_ttl_is_4() {
  match run_action(r#"spotatui.notify("hi")"#) {
    ScriptEffect::Notify(m, ttl) => {
      assert_eq!(m, "hi");
      assert_eq!(ttl, 4);
    }
    _ => panic!("expected a Notify effect"),
  }
}

// --- drain_effects: routes through App methods ---

#[cfg(test)]
mod drain_tests {
  use super::*;
  use crate::core::app::App;
  use crate::core::user_config::UserConfig;
  use crate::infra::network::IoEvent;
  use chrono::Duration as ChronoDuration;
  use rspotify::model::{
    context::{Actions, CurrentPlaybackContext},
    CurrentlyPlayingType, Device, DeviceType, PlayableItem, RepeatState,
  };
  use std::sync::mpsc::channel;
  use std::time::SystemTime;

  fn make_app() -> (App, std::sync::mpsc::Receiver<IoEvent>) {
    let (tx, rx) = channel();
    let app = App::new(tx, UserConfig::new(), SystemTime::now());
    (app, rx)
  }

  #[allow(deprecated)]
  fn make_device() -> Device {
    Device {
      id: Some("dev-test".to_string()),
      is_active: true,
      is_private_session: false,
      is_restricted: false,
      name: "Test Device".to_string(),
      _type: DeviceType::Computer,
      volume_percent: Some(50),
    }
  }

  #[allow(deprecated)]
  fn make_context(is_playing: bool, shuffle_state: bool) -> CurrentPlaybackContext {
    CurrentPlaybackContext {
      device: make_device(),
      repeat_state: RepeatState::Off,
      shuffle_state,
      context: None,
      timestamp: chrono::Utc::now(),
      progress: None,
      is_playing,
      item: None,
      currently_playing_type: CurrentlyPlayingType::Unknown,
      actions: Actions::default(),
    }
  }

  #[allow(deprecated)]
  fn make_context_with_track(is_playing: bool) -> CurrentPlaybackContext {
    use crate::core::test_helpers::full_track;
    let track = full_track("4uLU6hMCjMI75M1A2tKUQC", "Test Song");
    CurrentPlaybackContext {
      device: make_device(),
      repeat_state: RepeatState::Off,
      shuffle_state: false,
      context: None,
      timestamp: chrono::Utc::now(),
      progress: Some(ChronoDuration::milliseconds(0)),
      is_playing,
      item: Some(PlayableItem::Track(track)),
      currently_playing_type: CurrentlyPlayingType::Track,
      actions: Actions::default(),
    }
  }

  fn push_effect(engine: &ScriptEngine, effect: ScriptEffect) {
    engine.shared.effects.borrow_mut().push(effect);
  }

  #[test]
  fn drain_pause_while_playing_dispatches_pause_playback() {
    let (mut app, rx) = make_app();
    app.current_playback_context = Some(make_context(true, false));

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::Pause);
    engine.drain_effects(&mut app);

    match rx.try_recv() {
      Ok(IoEvent::PausePlayback) => {}
      _ => panic!("expected PausePlayback, got unexpected variant (IoEvent is not Debug)"),
    }
  }

  #[test]
  fn drain_pause_while_already_paused_is_noop() {
    let (mut app, rx) = make_app();
    app.current_playback_context = Some(make_context(false, false));

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::Pause);
    engine.drain_effects(&mut app);

    assert!(rx.try_recv().is_err(), "expected no IoEvent dispatched");
  }

  #[test]
  fn drain_play_while_paused_dispatches_start_playback() {
    let (mut app, rx) = make_app();
    app.current_playback_context = Some(make_context(false, false));

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::Play);
    engine.drain_effects(&mut app);

    match rx.try_recv() {
      Ok(IoEvent::StartPlayback(None, None, None)) => {}
      _ => panic!(
        "expected StartPlayback(None,None,None), got unexpected variant (IoEvent is not Debug)"
      ),
    }
  }

  #[test]
  fn drain_play_while_already_playing_is_noop() {
    let (mut app, rx) = make_app();
    app.current_playback_context = Some(make_context(true, false));

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::Play);
    engine.drain_effects(&mut app);

    assert!(rx.try_recv().is_err(), "expected no IoEvent dispatched");
  }

  #[test]
  fn drain_shuffle_true_when_off_dispatches_shuffle_true() {
    let (mut app, rx) = make_app();
    app.current_playback_context = Some(make_context(false, false));

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::SetShuffle(true));
    engine.drain_effects(&mut app);

    match rx.try_recv() {
      Ok(IoEvent::Shuffle(true)) => {}
      _ => panic!("expected Shuffle(true), got unexpected variant (IoEvent is not Debug)"),
    }
  }

  #[test]
  fn drain_shuffle_false_when_already_off_is_noop() {
    let (mut app, rx) = make_app();
    app.current_playback_context = Some(make_context(false, false));

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::SetShuffle(false));
    engine.drain_effects(&mut app);

    assert!(rx.try_recv().is_err(), "expected no IoEvent dispatched");
  }

  #[test]
  fn drain_set_volume_sets_pending_volume() {
    let (mut app, _rx) = make_app();

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::SetVolume(80));
    engine.drain_effects(&mut app);

    assert_eq!(app.pending_volume, Some(80));
  }

  #[test]
  fn drain_seek_with_track_context_dispatches_seek() {
    let (mut app, rx) = make_app();
    app.current_playback_context = Some(make_context_with_track(true));

    let engine = ScriptEngine::new().unwrap();
    push_effect(&engine, ScriptEffect::Seek(30_000));
    engine.drain_effects(&mut app);

    match rx.try_recv() {
      Ok(IoEvent::Seek(ms)) => assert_eq!(ms, 30_000),
      _ => panic!("expected Seek(30000), got unexpected variant (IoEvent is not Debug)"),
    }
  }

  #[test]
  fn drain_notify_error_sets_error_flag_on_app() {
    let (mut app, _rx) = make_app();

    let engine = ScriptEngine::new().unwrap();
    push_effect(
      &engine,
      ScriptEffect::NotifyError("plugin crashed".to_string(), 6),
    );
    engine.drain_effects(&mut app);

    assert_eq!(app.status_message.as_deref(), Some("plugin crashed"));
    assert!(app.status_message_is_error);
  }

  #[test]
  fn drain_notify_error_blocks_subsequent_normal_notify() {
    let (mut app, _rx) = make_app();

    let engine = ScriptEngine::new().unwrap();
    push_effect(
      &engine,
      ScriptEffect::NotifyError("error msg".to_string(), 6),
    );
    push_effect(&engine, ScriptEffect::Notify("normal msg".to_string(), 4));
    engine.drain_effects(&mut app);

    assert_eq!(app.status_message.as_deref(), Some("error msg"));
    assert!(app.status_message_is_error);
  }
}

// --- diff_events ---

#[test]
fn diff_none_to_some_is_track_change() {
  let new = Some(playback(Some(track("uri:1", "A")), true, 0));
  let q = Some(vec![]);
  let events = diff_events(&None, &None, &new, &q);
  assert!(events.contains(&ScriptEvent::TrackChange));
  // None -> playing also flips is_playing.
  assert!(events.contains(&ScriptEvent::PlaybackStateChange));
}

#[test]
fn diff_track_change_on_different_uri() {
  let old = Some(playback(Some(track("uri:1", "A")), true, 0));
  let new = Some(playback(Some(track("uri:2", "B")), true, 0));
  let q = Some(vec![]);
  let events = diff_events(&old, &q, &new, &q);
  assert!(events.contains(&ScriptEvent::TrackChange));
  assert!(!events.contains(&ScriptEvent::PlaybackStateChange));
}

#[test]
fn diff_play_pause_flip() {
  let old = Some(playback(Some(track("uri:1", "A")), true, 1000));
  let new = Some(playback(Some(track("uri:1", "A")), false, 1000));
  let q = Some(vec![]);
  let events = diff_events(&old, &q, &new, &q);
  assert_eq!(events, vec![ScriptEvent::PlaybackStateChange]);
}

#[test]
fn diff_seek_backward_beyond_threshold() {
  let old = Some(playback(Some(track("uri:1", "A")), true, 10_000));
  let new = Some(playback(Some(track("uri:1", "A")), true, 5_000));
  let q = Some(vec![]);
  let events = diff_events(&old, &q, &new, &q);
  assert!(events.contains(&ScriptEvent::Seek));
}

#[test]
fn diff_seek_forward_beyond_threshold() {
  let old = Some(playback(Some(track("uri:1", "A")), true, 1_000));
  let new = Some(playback(Some(track("uri:1", "A")), true, 9_000));
  let q = Some(vec![]);
  let events = diff_events(&old, &q, &new, &q);
  assert!(events.contains(&ScriptEvent::Seek));
}

#[test]
fn diff_small_forward_jump_is_not_seek() {
  // 3s forward jump is within Connect polling tolerance.
  let old = Some(playback(Some(track("uri:1", "A")), true, 1_000));
  let new = Some(playback(Some(track("uri:1", "A")), true, 4_000));
  let q = Some(vec![]);
  let events = diff_events(&old, &q, &new, &q);
  assert!(!events.contains(&ScriptEvent::Seek));
}

#[test]
fn diff_volume_change() {
  let old = Some(playback(Some(track("uri:1", "A")), true, 1_000));
  let mut new = playback(Some(track("uri:1", "A")), true, 1_000);
  new.volume_percent = Some(80);
  let q = Some(vec![]);
  let events = diff_events(&old, &q, &Some(new), &q);
  assert!(events.contains(&ScriptEvent::VolumeChange));
}

#[test]
fn diff_queue_change() {
  let old = Some(playback(Some(track("uri:1", "A")), true, 1_000));
  let new = old.clone();
  let old_q = Some(vec!["a".to_string()]);
  let new_q = Some(vec!["a".to_string(), "b".to_string()]);
  let events = diff_events(&old, &old_q, &new, &new_q);
  assert_eq!(events, vec![ScriptEvent::QueueChange]);
}
