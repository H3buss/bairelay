//! MQTT topic string helpers.
//!
//! All topics follow the pattern `{prefix}/{camera_name}/{category}/{subtopic}`
//! where `prefix` is the `mqtt.topic_prefix` config value (default
//! `"bairelay"`; `"neolink"` for drop-in migration from a legacy
//! deployment). Every helper is a pure function of `(prefix, cam)` —
//! no global state, no hidden defaults.

use crate::ai::AiObject;

// ── Status topics ──────────────────────────────────────────────────────

/// Camera online/offline status: `{prefix}/{cam}/status`
pub fn status(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status")
}

/// Availability topic of the bridge process itself: `{prefix}/status`.
/// Published `online` (retained) by the bridge on every broker ConnAck and
/// `offline` (retained) by the broker's last will if the process dies, so
/// Home Assistant's `unavailable` means « the bridge is gone » instead of
/// « the battery doorbell happens to be asleep ».
pub fn bridge_status(prefix: &str) -> String {
	format!("{prefix}/status")
}

/// Présence de la caméra : `{prefix}/{cam}/presence`.
///
/// `reachable` quand une tentative de joindre la caméra a réussi, `unreachable`
/// quand une tentative a échoué, après le déroulé normal de reconnexion ou de
/// wake. Topic distinct de [`status`] : une sonnette sur batterie ferme sa
/// session quelques instants après chaque wake, et brancher la disponibilité
/// des entités sur la session les griserait toutes pendant le sommeil. Message
/// conservé, publié seulement quand le verdict change, afin de ne pas écrire
/// cinquante fois la même chose sur un courtier domestique.
pub fn presence(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/presence")
}

/// Motion detection events: `{prefix}/{cam}/status/motion`
pub fn status_motion(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/motion")
}

/// Battery level percentage: `{prefix}/{cam}/status/battery_level`
pub fn status_battery_level(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/battery_level")
}

/// Full battery info XML (query response): `{prefix}/{cam}/status/battery`
pub fn status_battery(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/battery")
}

/// Preview image (base64 JPEG): `{prefix}/{cam}/status/preview`
pub fn status_preview(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/preview")
}

/// Floodlight state: `{prefix}/{cam}/status/floodlight`
pub fn status_floodlight(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/floodlight")
}

/// Floodlight task schedule: `{prefix}/{cam}/status/floodlight_tasks`
pub fn status_floodlight_tasks(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/floodlight_tasks")
}

/// PIR sensor state: `{prefix}/{cam}/status/pir`
pub fn status_pir(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/pir")
}

/// IR night-vision mode (`on` / `off` / `auto`): `{prefix}/{cam}/status/ir`
pub fn status_ir(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/ir")
}

/// Status LED on/off: `{prefix}/{cam}/status/led`
pub fn status_led(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/led")
}

/// AI detection state of one object class:
/// `{prefix}/{cam}/status/ai/{object}`
pub fn status_ai(prefix: &str, cam: &str, object: AiObject) -> String {
	format!("{prefix}/{cam}/status/ai/{}", object.topic_name())
}

/// Doorbell button press events: `{prefix}/{cam}/status/doorbell`
pub fn status_doorbell(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/doorbell")
}

/// Raw camera event announcement, as the camera pushed it over UDP:
/// `{prefix}/{cam}/status/event`
///
/// Non-retained, and the payload is the announcement's own JSON object
/// (`{"event":4,"duration":19281,"totalduration":2225330,"reason":19}`). This is
/// the diagnostic trail: when an `event`/`reason` value surprises us, the answer
/// is here instead of in a fresh capture off the camera.
pub fn status_event(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/event")
}

/// PTZ preset position: `{prefix}/{cam}/status/ptz/preset`
pub fn status_ptz_preset(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/status/ptz/preset")
}

// ── Control topics ─────────────────────────────────────────────────────

/// Floodlight on/off: `{prefix}/{cam}/control/floodlight`
pub fn control_floodlight(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/floodlight")
}

/// Floodlight schedule tasks: `{prefix}/{cam}/control/floodlight_tasks`
pub fn control_floodlight_tasks(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/floodlight_tasks")
}

/// Status LED on/off: `{prefix}/{cam}/control/led`
pub fn control_led(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/led")
}

/// IR night‐vision mode: `{prefix}/{cam}/control/ir`
pub fn control_ir(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/ir")
}

/// PIR sensor enable/disable: `{prefix}/{cam}/control/pir`
pub fn control_pir(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/pir")
}

/// Reboot camera: `{prefix}/{cam}/control/reboot`
pub fn control_reboot(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/reboot")
}

/// PTZ directional movement: `{prefix}/{cam}/control/ptz`
pub fn control_ptz(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/ptz")
}

/// PTZ go to preset: `{prefix}/{cam}/control/ptz/preset`
pub fn control_ptz_preset(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/ptz/preset")
}

/// PTZ assign current position to preset: `{prefix}/{cam}/control/ptz/assign`
pub fn control_ptz_assign(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/ptz/assign")
}

/// Zoom level control: `{prefix}/{cam}/control/zoom`
pub fn control_zoom(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/zoom")
}

/// Siren on/off: `{prefix}/{cam}/control/siren`
pub fn control_siren(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/siren")
}

/// Wake battery camera for N minutes: `{prefix}/{cam}/control/wakeup`
pub fn control_wakeup(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/control/wakeup")
}

// ── Query topics ───────────────────────────────────────────────────────

/// Request battery level: `{prefix}/{cam}/query/battery`
pub fn query_battery(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/query/battery")
}

/// Request preview snapshot: `{prefix}/{cam}/query/preview`
pub fn query_preview(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/query/preview")
}

/// Request PIR state: `{prefix}/{cam}/query/pir`
pub fn query_pir(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/query/pir")
}

/// Request PTZ preset list: `{prefix}/{cam}/query/ptz/preset`
pub fn query_ptz_preset(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/query/ptz/preset")
}

// ── Réglages (settings) ────────────────────────────────────────────────
//
// Les réglages suivent exactement la forme des topics existants, avec un
// segment de plus pour la famille : `status/<famille>/<feuille>` en lecture,
// `control/<famille>/<feuille>` en écriture. Une feuille par entité Home
// Assistant, parce qu'un topic JSON partagé ne se relit pas proprement après
// une écriture (et qu'un état conservé doit être le plus petit possible).

/// État d'un réglage : `{prefix}/{cam}/status/{family}/{leaf}`
pub fn status_setting(prefix: &str, cam: &str, family: &str, leaf: &str) -> String {
	format!("{prefix}/{cam}/status/{family}/{leaf}")
}

/// Le bloc complet d'une famille, en JSON brut de la caméra :
/// `{prefix}/{cam}/status/{family}/json`. Pour les automates et le
/// débogage ; les entités HA, elles, consomment les feuilles scalaires.
pub fn status_settings_json(prefix: &str, cam: &str, family: &str) -> String {
	format!("{prefix}/{cam}/status/{family}/json")
}

/// La feuille de diagnostic d'une famille : la raison lisible de son dernier
/// refus.
///
/// Effacer les états d'une famille dit qu'elle n'a pas de valeur, pas
/// **pourquoi**. Cette feuille porte une phrase courte — jamais le XML de la
/// caméra, jamais un identifiant, jamais un mot de passe (voir
/// [`crate::status::publishable_reason`], qui l'assainit avant publication).
///
/// Ce n'est pas une feuille d'entité : aucune entité Home Assistant ne la
/// découvre, et aucun `SettingCommand` ne l'accepte en écriture.
pub const REASON_LEAF: &str = "reason";

/// Motif du dernier refus d'une famille : `{prefix}/{cam}/status/{family}/reason`
pub fn status_setting_reason(prefix: &str, cam: &str, family: &str) -> String {
	status_setting(prefix, cam, family, REASON_LEAF)
}

/// Écriture d'un réglage : `{prefix}/{cam}/control/{family}/{leaf}`
pub fn control_setting(prefix: &str, cam: &str, family: &str, leaf: &str) -> String {
	format!("{prefix}/{cam}/control/{family}/{leaf}")
}

/// (Re)lecture de tous les réglages : `{prefix}/{cam}/query/settings`
pub fn query_settings(prefix: &str, cam: &str) -> String {
	format!("{prefix}/{cam}/query/settings")
}

/// (Re)lecture d'une famille : `{prefix}/{cam}/query/settings/{family}`
pub fn query_settings_family(prefix: &str, cam: &str, family: &str) -> String {
	format!("{prefix}/{cam}/query/settings/{family}")
}

// ── Subscribe list ─────────────────────────────────────────────────────

/// Returns all control + query topics that the bridge should subscribe to
/// for the given camera.
pub fn subscribe_topics(prefix: &str, cam: &str) -> Vec<String> {
	vec![
		// Control topics
		control_floodlight(prefix, cam),
		control_floodlight_tasks(prefix, cam),
		control_led(prefix, cam),
		control_ir(prefix, cam),
		control_pir(prefix, cam),
		control_reboot(prefix, cam),
		control_ptz(prefix, cam),
		control_ptz_preset(prefix, cam),
		control_ptz_assign(prefix, cam),
		control_zoom(prefix, cam),
		control_siren(prefix, cam),
		control_wakeup(prefix, cam),
		// Réglages : n'importe quelle feuille sous une famille. Le joker est
		// volontaire — la liste des familles et des feuilles valides est dans
		// `crate::settings`, pas ici ; une paire inconnue est rejetée au
		// parsing, pas par le filtre du broker. Il couvre aussi
		// `control/ptz/{preset,assign}`, déjà listés explicitement au-dessus.
		format!("{prefix}/{cam}/control/+/+"),
		// Query topics
		query_battery(prefix, cam),
		query_preview(prefix, cam),
		query_pir(prefix, cam),
		query_ptz_preset(prefix, cam),
		query_settings(prefix, cam),
		format!("{prefix}/{cam}/query/settings/+"),
	]
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_topics_of_settings_suivent_the_forme_existante() {
		assert_eq!(
			status_setting("bairelay", "cam1", "audio", "volume"),
			"bairelay/cam1/status/audio/volume"
		);
		assert_eq!(
			control_setting("bairelay", "cam1", "denoise", "level"),
			"bairelay/cam1/control/denoise/level"
		);
		assert_eq!(
			status_settings_json("bairelay", "cam1", "image"),
			"bairelay/cam1/status/image/json"
		);
		// Le motif du dernier refus est une feuille de la famille, au même
		// niveau que son JSON : rien de nouveau à apprendre au broker.
		assert_eq!(
			status_setting_reason("bairelay", "cam1", "audio"),
			"bairelay/cam1/status/audio/reason"
		);
		assert_eq!(
			status_setting("bairelay", "cam1", "audio", REASON_LEAF),
			status_setting_reason("bairelay", "cam1", "audio"),
			"le motif doit rester une feuille de la famille, pas un topic à part"
		);
		assert_eq!(query_settings("bairelay", "cam1"), "bairelay/cam1/query/settings");
		assert_eq!(
			query_settings_family("bairelay", "cam1", "pir"),
			"bairelay/cam1/query/settings/pir"
		);
	}

	#[test]
	fn the_list_of_abonnement_couvre_the_settings() {
		let topics = subscribe_topics("bairelay", "cam1");
		assert!(
			topics.contains(&"bairelay/cam1/control/+/+".to_string()),
			"les feuilles de réglages ne seraient jamais reçues : {topics:?}"
		);
		assert!(topics.contains(&"bairelay/cam1/query/settings".to_string()));
		assert!(topics.contains(&"bairelay/cam1/query/settings/+".to_string()));
		// Aucun doublon : un abonnement en trop est invisible mais ment sur le
		// nombre de souscriptions déclarées.
		let mut tri = topics.clone();
		tri.sort();
		tri.dedup();
		assert_eq!(tri.len(), topics.len(), "{topics:?}");
	}

	#[test]
	fn the_topic_of_presence_is_distinct_of_celui_of_the_session() {
		assert_eq!(presence("bairelay", "cam1"), "bairelay/cam1/presence");
		assert_eq!(presence("neolink", "porche"), "neolink/porche/presence");
		// La présence ne suit pas la session : les deux topics doivent rester
		// distincts, sinon une sonnette endormie griserait tout son tableau de
		// bord a chaque fermeture de session.
		assert_ne!(presence("bairelay", "cam1"), status("bairelay", "cam1"));
		assert_ne!(presence("bairelay", "cam1"), bridge_status("bairelay"));
	}
}
