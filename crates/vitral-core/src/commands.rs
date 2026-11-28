use crate::sync::base64_decode;
use crate::{
    clip::ClipboardPayload,
    eng::{SwipeDir, MOD_LANG_TOGGLE, MOD_VOICE_HOLD},
    hw::HwMode,
    imp::LexiconFormat,
    VitralCore,
};
use serde::de::DeserializeOwned;
use serde_json::{json, to_value, Value};
use std::{error::Error, path::Path};

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn arg<T: DeserializeOwned>(v: &Value, name: &str) -> Result<T> {
    Ok(serde_json::from_value(
        v.get(name)
            .ok_or_else(|| format!("missing {name}"))?
            .clone(),
    )?)
}

fn string<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v[name]
        .as_str()
        .ok_or_else(|| format!("expected string {name}").into())
}

fn keycode(key: &str) -> Result<u32> {
    Ok(match key {
        "space" | "Space" => 32,
        "Return" | "Enter" => 13,
        "Escape" => 27,
        "BackSpace" | "Backspace" => 8,
        "Home" => 0xff50,
        "Left" => 0xff51,
        "Right" => 0xff53,
        "Page_Up" | "PageUp" => 0xff55,
        "Page_Down" | "PageDown" => 0xff56,
        "End" => 0xff57,
        "Delete" => 0xffff,
        "" => 0,
        key if key
            .strip_prefix('F')
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| (1..=35).contains(&n)) =>
        {
            0xffbd + key[1..].parse::<u32>()?
        }
        "Shift" => 0xffe1,
        _ if key.chars().count() == 1 => key.chars().next().unwrap() as u32,
        _ => return Err(format!("unknown key {key}").into()),
    })
}

pub fn execute(core: &VitralCore, cmd: &str, v: &Value) -> Result<Value> {
    Ok(match cmd {
        "context-next-hints" => json!({"hints": core.context_next_hints(
            v["context_before"].as_str().unwrap_or(""), v["prediction_text"].as_str(),
        )}),
        "predict-request" => to_value(core.predict_request(serde_json::from_value(v.clone())?))?,
        "predict-poll" => to_value(core.predict_poll(v.get("id").map(|_| arg(v, "id")).transpose()?))?,
        "predict-cancel" => to_value(core.predict_cancel())?,
        "predict-accept" => json!({"accepted":core.predict_accept(arg(v, "id")?, string(v, "text")?)?}),
        "predict-dismiss" => json!({"dismissed":core.predict_dismiss_with_text(
            arg(v, "id")?,
            v.get("text").filter(|value| !value.is_null()).map(|_| string(v, "text")).transpose()?,
        )?}),
        "profile-state" => core.profile_state(),
        "profile-distill" => { core.profile_distill()?; core.profile_state() },
        "profile-update" => { core.profile_update(serde_json::from_value(v.clone())?)?; core.profile_state() },
        "profile-clear" => { core.profile_clear()?; core.profile_state() },
        "configure" => {
            let clear_pw = v.get("clear_koofr_password").and_then(Value::as_bool).unwrap_or(false);
            core.apply_preferences_clearing_secrets(serde_json::from_value(v.clone())?, clear_pw)?;
            json!({"ok":true,"shortcuts":core.input.lock().engine.config.shortcuts})
        }
        "personalization-stats" => to_value(core.personalization_stats())?,
        "personalization-clear" => to_value(core.clear_learned_habits())?,
        "storage" => to_value(core.cloud_usage()?)?,
        "set-pure-offline" => {
            core.set_pure_offline(arg(v, "enabled")?);
            json!({"pure_offline":core.is_pure_offline()})
        }
        "is-pure-offline" => json!({"pure_offline":core.is_pure_offline()}),
        "set-power-save" => {
            core.set_power_save(arg(v, "enabled")?);
            json!({"power_save":core.is_power_save()})
        }
        "set-schema" => {
            core.set_schema(arg(v, "schema")?);
            json!({"schema":v["schema"]})
        }
        "set-opencc-mode" => {
            core.set_opencc_mode(arg(v, "mode")?);
            json!({"mode":v["mode"]})
        }
        "models-state" => to_value(core.models_state())?,
        "models-download" => to_value(core.models_download(string(v, "id")?)?)?,
        "models-cancel" => to_value(core.models_cancel(string(v, "id")?)?)?,
        "models-set-enabled" => to_value(core.models_set_enabled(string(v, "id")?, arg(v, "enabled")?)?)?,
        "models-delete" => to_value(core.models_delete(string(v, "id")?)?)?,
        "models-refresh-catalog" => to_value(core.models_refresh_catalog(v.get("url").filter(|v| !v.is_null()).map(|_| string(v, "url")).transpose()?)?)?,
        "reload-assets" => to_value(core.reload_assets()?)?,
        "on-app-focus" => to_value(core.on_app_focus_with_learning_flag(
            string(v, "app_id")?,
            v.get("no_personalized_learning").map(|_| arg(v, "no_personalized_learning")).transpose()?.unwrap_or(false),
        )?)?,
        "reset-commit-context" => { core.reset_commit_context(); json!({"ok":true}) },
        "notify-deleted-chars" => { core.notify_deleted_chars(arg(v, "count")?); json!({"ok":true}) },
        "process-key" => {
            let mut flags = 0;
            for m in v["modifiers"].as_array().into_iter().flatten() {
                flags |= match m.as_str() {
                    Some("lang_toggle") => MOD_LANG_TOGGLE,
                    Some("voice_hold") => MOD_VOICE_HOLD,
                    Some("Shift" | "shift") => 1,
                    Some("Control" | "ctrl") => 4,
                    Some("Alt" | "alt") => 8,
                    _ => return Err(format!("unknown modifier {m}").into()),
                };
            }
            let swipe = if v["swipe_dir"].is_null() {
                SwipeDir::None
            } else {
                arg(v, "swipe_dir")?
            };
            to_value(core.process_key(keycode(string(v, "keycode")?)?, flags, swipe)?)?
        }
        "shortcuts-state" => {
            let st = core.input.lock();
            json!({"bindings":st.engine.config.shortcuts,"actions":crate::shortcuts::ShortcutAction::ALL.map(|action| json!({"id":action,"label":action.label()}))})
        }
        "shortcut-match" => {
            let key = crate::shortcuts::normalize_key(string(v, "key")?)?;
            let st = core.input.lock();
            to_value(st.engine.config.shortcuts.iter().find(|b| !b.key.is_empty() && b.key == key).map(|b| b.action))?
        }
        "shortcut-key" => to_value(core.shortcut_key(string(v, "key")?)?)?,
        "set-syllable-split" => to_value(core.set_syllable_split(arg(v, "enabled")?)?)?,
        "set-preedit-cursor" => to_value(core.set_preedit_cursor(arg(v, "cursor_utf8")?)?)?,
        "candidate-state" => to_value(core.candidate_state()?)?,
        "select-candidate" => to_value(core.select_candidate(arg(v, "idx")?)?)?,
        "pin-candidate" => to_value(core.pin_candidate(arg(v, "idx")?)?)?,
        "erase-candidate" => to_value(core.erase_candidate(arg(v, "idx")?)?)?,
        "hw-begin" => {
            let mode = match v["mode"].as_str() {
                Some("ContinuousLattice") => HwMode::ContinuousLattice,
                Some("PhoneticLookup") => HwMode::PhoneticLookup,
                _ => arg(v, "mode")?,
            };
            core.hw_begin(mode);
            json!({"ok":true})
        }
        "hw-push-stroke" => {
            to_value(core.hw_push_stroke(&arg::<Vec<_>>(v, "points")?, arg(v, "pen_up")?)?)?
        }
        "hw-end" => to_value(core.hw_end()?)?,
        "hw-clear" => {
            core.hw_clear();
            json!({"ok":true})
        }
        "hw-override-slot" => {
            to_value(core.hw_override_slot(arg(v, "slot_idx")?, arg(v, "candidate_idx")?)?)?
        }
        "hw-confirm" => json!({"commit_text": core.hw_confirm()?}),
        "import-lexicon" => {
            let format = match v["format_hint"].as_str() {
                None => LexiconFormat::Auto,
                Some("dict.yaml") => LexiconFormat::DictYaml,
                _ => arg(v, "format_hint")?,
            };
            to_value(core.import_lexicon(Path::new(string(v, "path")?), format)?)?
        }
        "clipboard-push" | "clipboard-ingest-remote" => {
            let payload: ClipboardPayload = if v["payload"].is_string() {
                serde_json::from_value(
                    json!({
                        "content": v["payload"],
                        "source_device": v["source_device"].as_str().unwrap_or(""),
                    }),
                )?
            } else {
                arg(v, "payload")?
            };
            if cmd == "clipboard-push" {
                to_value(core.clipboard_push(payload))?
            } else {
                to_value(core.clipboard_ingest_remote(payload, arg(v, "is_cold_boot")?))?
            }
        }
        "clipboard-state" => to_value(core.clipboard_state())?,
        "sync-status" => to_value(core.sync_status())?,
        "cloud-check" => { core.check_cloud_connection()?; json!({"connected": true}) },
        "poll-services" => to_value(core.poll_services()?)?,
        "sync-now" => to_value(core.sync_now()?)?,
        "clipboard-poll" => to_value(core.poll_clipboard()?)?,
        "clipboard-poll-current" => to_value(core.poll_clipboard_current()?)?,
        "clipboard-lan-revision" => json!(core.clipboard_lan_revision()),
        "hide-quick-paste-for-input" => { core.hide_quick_paste_for_input(); to_value(core.quick_paste_state())? },
        "quick-paste-state" => to_value(core.quick_paste_state())?,
        "consume-quick-paste" => to_value(core.consume_quick_paste())?,
        "on-ime-open" => to_value(core.on_ime_open())?,
        "on-ime-close" => to_value(core.on_ime_close())?,
        "dismiss-quick-paste" => to_value(core.dismiss_quick_paste())?,
        "clipboard-pin" => {
            let code = v.get("shortcut_code").map(|code| {
                if code.is_null() { Ok(None) } else { string(v, "shortcut_code").map(Some) }
            }).transpose()?;
            to_value(core.clipboard_pin_update(string(v, "id")?, code)?)?
        }
        "clipboard-unpin" => to_value(core.clipboard_unpin(string(v, "id")?)?)?,
        "clipboard-clear-unpinned" => to_value(core.clipboard_clear_unpinned())?,
        "relay-discover" => to_value(core.discover_devices()?)?,
        "relay-stop-discovery" => {
            core.stop_discovery();
            json!({"ok": true})
        }
        "relay-devices-mru" => to_value(core.relay_devices_mru())?,
        "relay-send" => {
            let data = string(v, "path_or_text")?;
            to_value(core.relay_send(string(v, "device_id")?, data, Path::new(data).is_file())?)?
        }
        "relay-poll-inbox" => to_value(core.relay_poll_inbox()?)?,
        "relay-cleanup-orphans" => to_value(core.relay_cleanup_orphans()?)?,
        "voice-start" => {
            core.voice_start();
            json!({"ok":true})
        }
        "voice-push-pcm" => {
            let bytes = base64_decode(string(v, "pcm_i16_b64")?)?;
            if bytes.len() % 2 != 0 {
                return Err("incomplete PCM sample".into());
            }
            let samples: Vec<_> = bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            core.voice_push_pcm(&samples)?;
            json!({"sample_count":samples.len()})
        }
        "voice-preview" => {
            let preview = core.voice_preview()?;
            json!({
                "transcript": preview.transcript,
                "language": preview.language,
                "duration_ms": preview.duration_ms,
                "sample_count": preview.sample_count,
            })
        }
        "voice-stop" => to_value(core.voice_stop()?)?,
        "llm-unload" => {
            core.unload_idle_models();
            json!({"ok":true})
        }
        "ai-inline" => to_value(core.ai_inline_in_app(
            arg(v, "mode")?,
            string(v, "text")?,
            string(v, "target_lang")?,
            v.get("app_id").filter(|value| !value.is_null()).map(|_| string(v, "app_id")).transpose()?,
        )?)?,
        "ai-inline-route" => json!({"route":core.ai_inline_route(), "pure_offline":core.is_pure_offline()}),
        "ai-inline-start" => to_value(core.ai_inline_start(
            arg(v, "mode")?,
            string(v, "text")?,
            string(v, "target_lang")?,
            v.get("app_id").filter(|value| !value.is_null()).map(|_| string(v, "app_id")).transpose()?,
        )?)?,
        "ai-inline-poll" => to_value(core.ai_inline_poll(arg(v, "id")?)?)?,
        "ai-inline-cancel" => json!({"cancelled":core.ai_inline_cancel(v.get("id").map(|_| arg(v, "id")).transpose()?.flatten())}),
        "privacy-audit" => core.privacy_audit(),
        "privacy-audit-clear" => { core.privacy_audit_clear(); json!({"ok":true}) },
        "restore-from-koofr" => {
            let mut result = to_value(core.restore_from_koofr(&v["config"].to_string())?)?;
            let st = core.input.lock();
            result["preferences"] = json!({
                "schema": st.engine.schema, "opencc_mode": st.engine.opencc_mode, "shortcuts": st.engine.config.shortcuts,
                "clipboard_ring_capacity": st.engine.config.clipboard_ring_capacity,
                "frecency_half_life_sec": st.retention.half_life_sec
            });
            result
        }
        _ => return Err(format!("unknown command {cmd}").into()),
    })
}
