use std::rc::Rc;

use mlua::{Lua, LuaSerdeExt, Value};

use crate::core::plugin_api;

use super::effects::ScriptEffect;
use super::events::VALID_EVENT_NAMES;
use super::shared::{ScriptShared, HANDLERS_KEY};

/// Build the `spotatui` global table and its functions.
pub(super) fn install_api(lua: &Lua, shared: &Rc<ScriptShared>) -> mlua::Result<()> {
  let tbl = lua.create_table()?;

  tbl.set("api_version", plugin_api::API_VERSION)?;

  // spotatui.on(event, fn)
  {
    let lua_inner = lua.clone();
    let shared = shared.clone();
    let on = lua.create_function(move |_, (event, callback): (String, mlua::Function)| {
      if !VALID_EVENT_NAMES.contains(&event.as_str()) {
        return Err(mlua::Error::RuntimeError(format!(
          "spotatui.on: unknown event '{event}'; valid events: {}",
          VALID_EVENT_NAMES.join(", ")
        )));
      }
      let handlers: mlua::Table = lua_inner.named_registry_value(HANDLERS_KEY)?;
      let list: mlua::Table = match handlers.get::<Option<mlua::Table>>(event.clone())? {
        Some(t) => t,
        None => {
          let t = lua_inner.create_table()?;
          handlers.set(event.clone(), t.clone())?;
          t
        }
      };
      let entry = lua_inner.create_table()?;
      entry.set("plugin", shared.current_plugin.borrow().clone())?;
      entry.set("callback", callback)?;
      list.push(entry)?;
      Ok(())
    })?;
    tbl.set("on", on)?;
  }

  // Reads: spotatui.playback() / current_track() / devices()
  {
    let shared_pb = shared.clone();
    let playback = lua.create_function(move |lua, ()| {
      let pb = shared_pb.playback.borrow().clone();
      match pb {
        Some(state) => lua.to_value(&state),
        None => Ok(Value::Nil),
      }
    })?;
    tbl.set("playback", playback)?;

    let shared_ct = shared.clone();
    let current_track = lua.create_function(move |lua, ()| {
      let pb = shared_ct.playback.borrow().clone();
      match pb.and_then(|s| s.track) {
        Some(track) => lua.to_value(&track),
        None => Ok(Value::Nil),
      }
    })?;
    tbl.set("current_track", current_track)?;

    let shared_dev = shared.clone();
    let devices = lua.create_function(move |lua, ()| {
      let devices = shared_dev.devices.borrow().clone();
      lua.to_value(&devices)
    })?;
    tbl.set("devices", devices)?;
  }

  // Actions: queue effects.
  install_action(lua, &tbl, shared, "play", || ScriptEffect::Play)?;
  install_action(lua, &tbl, shared, "pause", || ScriptEffect::Pause)?;
  install_action(lua, &tbl, shared, "next", || ScriptEffect::Next)?;
  install_action(lua, &tbl, shared, "previous", || ScriptEffect::Previous)?;

  {
    let shared = shared.clone();
    let seek = lua.create_function(move |_, ms: u32| {
      shared.effects.borrow_mut().push(ScriptEffect::Seek(ms));
      Ok(())
    })?;
    tbl.set("seek", seek)?;
  }

  {
    let shared = shared.clone();
    let set_volume = lua.create_function(move |_, pct: i64| {
      let clamped = pct.clamp(0, 100) as u8;
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::SetVolume(clamped));
      Ok(())
    })?;
    tbl.set("set_volume", set_volume)?;
  }

  {
    let shared = shared.clone();
    let shuffle = lua.create_function(move |_, on: bool| {
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::SetShuffle(on));
      Ok(())
    })?;
    tbl.set("shuffle", shuffle)?;
  }

  {
    let shared = shared.clone();
    let search = lua.create_function(move |_, query: String| {
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::Search(query));
      Ok(())
    })?;
    tbl.set("search", search)?;
  }

  {
    let shared = shared.clone();
    let notify = lua.create_function(move |_, (msg, ttl): (String, Option<u64>)| {
      shared
        .effects
        .borrow_mut()
        .push(ScriptEffect::Notify(msg, ttl.unwrap_or(4)));
      Ok(())
    })?;
    tbl.set("notify", notify)?;
  }

  {
    let log = lua.create_function(move |_, msg: String| {
      log::info!("[lua] {msg}");
      Ok(())
    })?;
    tbl.set("log", log)?;
  }

  lua.globals().set("spotatui", tbl)?;
  Ok(())
}

/// Install a no-argument action that pushes a fixed effect.
pub(super) fn install_action(
  lua: &Lua,
  tbl: &mlua::Table,
  shared: &Rc<ScriptShared>,
  name: &str,
  make: fn() -> ScriptEffect,
) -> mlua::Result<()> {
  let shared = shared.clone();
  let f = lua.create_function(move |_, ()| {
    shared.effects.borrow_mut().push(make());
    Ok(())
  })?;
  tbl.set(name, f)?;
  Ok(())
}
