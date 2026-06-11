use crate::core::app::App;
use crate::core::plugin_api;
use crate::infra::network::IoEvent;

/// An action queued by a plugin, drained by the runner while holding `&mut App`.
///
/// Each variant routes through the same `App` methods as the equivalent keybinding,
/// so native-streaming fast paths and throttling/coalescing are automatically honoured.
/// Tests inspect effects via pattern matching (no `derive` needed).
pub(crate) enum ScriptEffect {
  Play,
  Pause,
  Next,
  Previous,
  Seek(u32),
  SetVolume(u8),
  SetShuffle(bool),
  /// Resolved at drain time (country lookup needs `App`).
  Search(String),
  /// message, ttl_secs
  Notify(String, u64),
  /// Error message, ttl_secs -- always shown; blocks normal message overwrites until it expires.
  NotifyError(String, u64),
}

/// Returns `true` when the current playback state indicates active playback.
pub(super) fn effective_is_playing(app: &App) -> bool {
  plugin_api::playback_state(app)
    .map(|p| p.is_playing)
    .unwrap_or(false)
}

/// Drain queued effects into the app while holding `&mut App`.
pub(super) fn apply_effects(effects: Vec<ScriptEffect>, app: &mut App) {
  for effect in effects {
    match effect {
      ScriptEffect::Play => {
        if !effective_is_playing(app) {
          app.toggle_playback();
        }
      }
      ScriptEffect::Pause => {
        if effective_is_playing(app) {
          app.toggle_playback();
        }
      }
      ScriptEffect::Next => app.next_track(),
      ScriptEffect::Previous => app.previous_track(),
      ScriptEffect::Seek(ms) => app.seek_to(ms),
      ScriptEffect::SetVolume(v) => app.set_volume_percent(v),
      ScriptEffect::SetShuffle(desired) => {
        let current = plugin_api::playback_state(app)
          .map(|p| p.shuffle)
          .unwrap_or(false);
        if current != desired {
          app.shuffle();
        }
      }
      ScriptEffect::Search(query) => {
        let country = app.get_user_country();
        app.dispatch(IoEvent::GetSearchResults(query, country));
      }
      ScriptEffect::Notify(msg, ttl) => app.set_status_message(msg, ttl),
      ScriptEffect::NotifyError(msg, ttl) => app.set_error_status_message(msg, ttl),
    }
  }
}
