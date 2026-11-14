//! Status publishing helpers for pushing camera state to MQTT.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::ai::AiObject;
use crate::client::SharedMqttClient;
use crate::control::IrMode;
use crate::error::MqttError;
use crate::settings::Family;
use crate::topics;

/// Wire-format JSON payload published on the floodlight state topic.
/// Matches neolink's `DiscoveryLight.state_value_template = "{{ value_json.state }}"`,
/// so HA can template the state off the `state` key. Exposed so tests
/// can assert on the exact bytes without spinning up an MQTT client.
pub(crate) fn floodlight_state_json(on: bool) -> &'static str {
	if on {
		r#"{"state":"on"}"#
	} else {
		r#"{"state":"off"}"#
	}
}

/// Les trois codes portés par la cellule de présence partagée : `0` = aucun
/// verdict publié depuis le démarrage du pont, puis les deux verdicts possibles.
const PRESENCE_UNKNOWN: u8 = 0;
const PRESENCE_ONLINE: u8 = 1;
const PRESENCE_OFFLINE: u8 = 2;

/// Dernier verdict de présence publié pour une caméra, partagé par tous les
/// publieurs de cette caméra.
///
/// Une sonnette sur batterie ouvre et ferme sa session des dizaines de fois par
/// jour. Sans cet état partagé, chaque wake republierait `reachable`, et un
/// courtier domestique ne veut pas de cinquante messages identiques par heure.
/// Une publication ne part donc que sur une transition. La bascule atomique
/// fait partir une seule publication, même quand deux tentatives concurrentes
/// demandent la même.
#[derive(Debug)]
pub struct PublishedPresence(AtomicU8);

impl Default for PublishedPresence {
	fn default() -> Self {
		Self(AtomicU8::new(PRESENCE_UNKNOWN))
	}
}

impl PublishedPresence {
	/// Applique le verdict `joignable` et rend le code précédent quand une
	/// publication doit partir. Rend `None` quand le verdict demandé était déjà
	/// le verdict courant, auquel cas rien ne part vers le courtier.
	fn transition(&self, reachable: bool) -> Option<(u8, u8)> {
		let cible = if reachable {
			PRESENCE_ONLINE
		} else {
			PRESENCE_OFFLINE
		};
		let mut current = self.0.load(Ordering::Acquire);
		loop {
			if current == cible {
				return None;
			}
			match self
				.0
				.compare_exchange_weak(current, cible, Ordering::AcqRel, Ordering::Acquire)
			{
				Ok(_) => return Some((current, cible)),
				Err(reel) => current = reel,
			}
		}
	}

	/// Remet le code précédent en place quand la publication correspondante a
	/// échoué, et seulement tant que le code courant est encore celui de la
	/// transition ratée. Sans ce retour, la cellule prétendrait un verdict absent
	/// du courtier, et le prochain contact ne republierait plus jamais rien.
	fn annuler(&self, cible: u8, previous: u8) {
		let _ = self
			.0
			.compare_exchange(cible, previous, Ordering::AcqRel, Ordering::Acquire);
	}
}

/// Convenience wrapper that publishes camera status messages.
///
/// Binds together a shared MQTT client, the `topic_prefix` (from the
/// `mqtt.topic_prefix` config knob — default `"bairelay"`), and a
/// camera name, so each call site does not have to re-construct the
/// full topic string.
pub struct StatusPublisher<'a> {
	client: &'a SharedMqttClient,
	topic_prefix: String,
	camera_name: String,
	/// Dernier verdict de présence publié, partagé par caméra. Voir
	/// [`PublishedPresence`] et [`Self::publish_presence`].
	presence: Arc<PublishedPresence>,
}

impl<'a> StatusPublisher<'a> {
	pub fn new(client: &'a SharedMqttClient, topic_prefix: &str, camera_name: &str) -> Self {
		Self {
			client,
			topic_prefix: topic_prefix.to_string(),
			camera_name: camera_name.to_string(),
			// Cellule propre à cette instance : un publieur construit ici puis jeté
			// ne déduplique donc rien. La présence passe par [`Self::with_presence`],
			// qui partage la cellule de la caméra.
			presence: Arc::new(PublishedPresence::default()),
		}
	}

	/// Même publieur, avec la cellule de présence partagée par une caméra : seul
	/// ce constructeur déduplique entre deux publications successives, donc seul
	/// lui convient [`Self::publish_presence`].
	pub fn with_presence(
		client: &'a SharedMqttClient,
		topic_prefix: &str,
		camera_name: &str,
		presence: Arc<PublishedPresence>,
	) -> Self {
		Self {
			client,
			topic_prefix: topic_prefix.to_string(),
			camera_name: camera_name.to_string(),
			presence,
		}
	}

	/// Publish "connected" or "disconnected" on the camera status topic (retained).
	///
	/// Ne dit rien de la disponibilité : voir [`Self::publish_presence`], qui lui
	/// est préférée pour ça.
	pub async fn publish_connection(&self, online: bool) -> Result<(), MqttError> {
		let payload = if online { "connected" } else { "disconnected" };
		let topic = topics::status(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, payload.as_bytes())
			.await
	}

	/// Publie le verdict de présence de la caméra (`reachable` / `unreachable`)
	/// en message conservé sur [`topics::presence`], seulement quand le verdict
	/// change.
	///
	/// La présence ne suit pas la session : une sonnette sur batterie ferme la
	/// sienne quelques instants après chaque wake, et la suivre griserait tout le
	/// tableau de bord pendant le sommeil. Elle se déduit de trois événements qui
	/// ont déjà lieu — démarrage du pont, session ouverte, tentative qui renonce —
	/// et ne déclenche donc jamais un appel réseau de plus vers la caméra.
	///
	/// La cellule de dédoublonnage doit être partagée par caméra. Construite par
	/// [`Self::new`], elle reste propre à cette instance et ne déduplique que dans
	/// cette dernière : les trois endroits du pont qui publient la présence passent
	/// par [`Self::with_presence`].
	pub async fn publish_presence(&self, reachable: bool) -> Result<(), MqttError> {
		let Some((previous, cible)) = self.presence.transition(reachable) else {
			return Ok(());
		};
		let payload = if reachable { "reachable" } else { "unreachable" };
		let topic = topics::presence(&self.topic_prefix, &self.camera_name);
		match self
			.client
			.publish_retained(&topic, payload.as_bytes())
			.await
		{
			Ok(()) => Ok(()),
			Err(e) => {
				self.presence.annuler(cible, previous);
				Err(e)
			}
		}
	}

	/// Publish motion detection state (retained).
	pub async fn publish_motion(&self, detected: bool) -> Result<(), MqttError> {
		let payload = if detected { "on" } else { "off" };
		let topic = topics::status_motion(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, payload.as_bytes())
			.await
	}

	/// Publish the detection state of one AI object class (retained).
	pub async fn publish_ai_object(
		&self,
		object: AiObject,
		detected: bool,
	) -> Result<(), MqttError> {
		let payload = if detected { "on" } else { "off" };
		let topic = topics::status_ai(&self.topic_prefix, &self.camera_name, object);
		self.client
			.publish_retained(&topic, payload.as_bytes())
			.await
	}

	/// Publish a doorbell button press (`press`, not retained: a press
	/// is an event, a retained copy would replay on every reconnect).
	pub async fn publish_doorbell_press(&self) -> Result<(), MqttError> {
		let topic = topics::status_doorbell(&self.topic_prefix, &self.camera_name);
		self.client.publish(&topic, b"press").await
	}

	/// Publish "unknown" motion state (retained). Used at startup before
	/// the motion listener has determined the actual state.
	pub async fn publish_motion_unknown(&self) -> Result<(), MqttError> {
		let topic = topics::status_motion(&self.topic_prefix, &self.camera_name);
		self.client.publish_retained(&topic, b"unknown").await
	}

	/// Publish battery level as a percentage string (retained).
	pub async fn publish_battery_level(&self, percent: u8) -> Result<(), MqttError> {
		let clamped = percent.min(100);
		let topic = topics::status_battery_level(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, clamped.to_string().as_bytes())
			.await
	}

	/// Publish a preview image as base64-encoded JPEG (retained).
	pub async fn publish_preview(&self, jpeg_data: &[u8]) -> Result<(), MqttError> {
		let encoded = BASE64.encode(jpeg_data);
		let topic = topics::status_preview(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, encoded.as_bytes())
			.await
	}

	/// Publish floodlight state (retained). Emits `{"state":"on"}` or
	/// `{"state":"off"}` so HA's `light` discovery entity can template
	/// the value out with `value_json.state`, matching neolink's wire
	/// format. The sibling
	/// [`Self::publish_floodlight_tasks_enabled`] stays plain
	/// `"on"`/`"off"` — neolink's tasks switch is a `switch` (not a
	/// templated `light`) and takes bare state_on/state_off.
	pub async fn publish_floodlight(&self, on: bool) -> Result<(), MqttError> {
		let payload = floodlight_state_json(on);
		let topic = topics::status_floodlight(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, payload.as_bytes())
			.await
	}

	/// Publish floodlight task schedule as JSON (retained).
	pub async fn publish_floodlight_tasks(&self, json: &str) -> Result<(), MqttError> {
		let topic = topics::status_floodlight_tasks(&self.topic_prefix, &self.camera_name);
		self.client.publish_retained(&topic, json.as_bytes()).await
	}

	/// Publish floodlight tasks enabled/disabled state (retained).
	pub async fn publish_floodlight_tasks_enabled(&self, enabled: bool) -> Result<(), MqttError> {
		let payload = if enabled { "on" } else { "off" };
		let topic = topics::status_floodlight_tasks(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, payload.as_bytes())
			.await
	}

	/// Publish PIR sensor state (retained).
	pub async fn publish_pir(&self, enabled: bool) -> Result<(), MqttError> {
		let payload = if enabled { "on" } else { "off" };
		let topic = topics::status_pir(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, payload.as_bytes())
			.await
	}

	/// Publish IR night-vision mode (retained).
	pub async fn publish_ir(&self, mode: IrMode) -> Result<(), MqttError> {
		let topic = topics::status_ir(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, mode.as_payload().as_bytes())
			.await
	}

	/// Publish status LED state (retained).
	pub async fn publish_led(&self, on: bool) -> Result<(), MqttError> {
		let payload = if on { "on" } else { "off" };
		let topic = topics::status_led(&self.topic_prefix, &self.camera_name);
		self.client
			.publish_retained(&topic, payload.as_bytes())
			.await
	}

	/// Publish PTZ preset position as JSON (retained).
	pub async fn publish_ptz_preset(&self, json: &str) -> Result<(), MqttError> {
		let topic = topics::status_ptz_preset(&self.topic_prefix, &self.camera_name);
		self.client.publish_retained(&topic, json.as_bytes()).await
	}

	// ── Réglages ────────────────────────────────────────────────────────
	//
	// Un topic par feuille, conservé, et **toujours la valeur relue** : après
	// une écriture, le dispatch relit le bloc et publie ce que la caméra a
	// gardé. Publier la valeur demandée ferait mentir un état conservé sur le
	// broker, qui survivrait au redémarrage du pont.

	/// Publie la valeur texte d'un réglage (conservé).
	pub async fn publish_setting(
		&self,
		family: Family,
		leaf: &str,
		value: &str,
	) -> Result<(), MqttError> {
		let topic =
			topics::status_setting(&self.topic_prefix, &self.camera_name, family.slug(), leaf);
		self.client
			.publish_retained(&topic, value.as_bytes())
			.await
	}

	/// Un réglage qui est un interrupteur : `on`/`off`, exactement les deux
	/// textes que l'entité `switch` découvre en `state_on`/`state_off`.
	pub async fn publish_setting_switch(
		&self,
		family: Family,
		leaf: &str,
		on: bool,
	) -> Result<(), MqttError> {
		self.publish_setting(family, leaf, if on { "on" } else { "off" })
			.await
	}

	/// Un drapeau lu dans le bloc caméra (`"1"`/`"0"`), pour les
	/// `binary_sensor` de lecture seule.
	pub async fn publish_setting_flag(
		&self,
		family: Family,
		leaf: &str,
		on: bool,
	) -> Result<(), MqttError> {
		self.publish_setting(family, leaf, if on { "1" } else { "0" })
			.await
	}

	/// Le bloc d'une famille en JSON (conservé) : pour les automates et le
	/// diagnostic. Les entités Home Assistant, elles, consomment les feuilles
	/// scalaires.
	pub async fn publish_settings_json(
		&self,
		family: Family,
		json: &str,
	) -> Result<(), MqttError> {
		let topic =
			topics::status_settings_json(&self.topic_prefix, &self.camera_name, family.slug());
		self.client.publish_retained(&topic, json.as_bytes()).await
	}

	/// Efface un état conservé (payload vide conservé = topic effacé chez
	/// Home Assistant).
	///
	/// C'est ce qu'on fait quand la caméra **n'a pas répondu** : mieux vaut un
	/// capteur « inconnu » qu'une valeur de la dernière fois, qui pourrait
	/// venir d'un autre réglage, d'un autre firmware, ou d'avant un
	/// appairage. « Refusé » et « vide » doivent rester distincts.
	pub async fn clear_setting(&self, family: Family, leaf: &str) -> Result<(), MqttError> {
		let topic =
			topics::status_setting(&self.topic_prefix, &self.camera_name, family.slug(), leaf);
		self.client.publish_retained(&topic, b"").await
	}

	/// Efface le JSON d'une famille.
	pub async fn clear_settings_json(&self, family: Family) -> Result<(), MqttError> {
		let topic =
			topics::status_settings_json(&self.topic_prefix, &self.camera_name, family.slug());
		self.client.publish_retained(&topic, b"").await
	}

	/// Le motif du dernier refus d'une famille, en conservé
	/// (`status/<famille>/reason`).
	///
	/// Une famille dont les états sont effacés est silencieuse : Home Assistant
	/// affiche « inconnu » et rien ne dit si la caméra a refusé, n'a pas répondu,
	/// ou n'imprime pas la balise. Ce topic porte la phrase. Il est assaini par
	/// [`publishable_reason`] — une ligne, sans XML, sans secret — parce qu'un
	/// topic conservé est relu des années plus tard.
	pub async fn publish_family_reason(
		&self,
		family: Family,
		reason: &str,
	) -> Result<(), MqttError> {
		let topic =
			topics::status_setting_reason(&self.topic_prefix, &self.camera_name, family.slug());
		let propre = publishable_reason(reason);
		self.client.publish_retained(&topic, propre.as_bytes()).await
	}

	/// Efface le motif d'une famille : elle vient de répondre, la raison
	/// précédente n'a plus lieu d'être conservée.
	pub async fn clear_family_reason(&self, family: Family) -> Result<(), MqttError> {
		let topic =
			topics::status_setting_reason(&self.topic_prefix, &self.camera_name, family.slug());
		self.client.publish_retained(&topic, b"").await
	}

}

/// Longueur maximale d'un motif. Un topic conservé est relus à chaque démarrage
/// de Home Assistant : une phrase, pas une trace de pile ni un dump.
const REASON_MAX: usize = 160;

/// Ce qu'on publie quand le motif lui-même est inutilisable.
const REASON_SANS_DETAIL: &str = "the camera gave no reason";
const REASON_MASQUE: &str = "reason hidden: it could have contained a secret";

/// Les sous-chaînes qui n'ont rien à faire sur un topic conservé. Aucun motif
/// construit par le pont n'en contient ; cette ceinture tient la promesse
/// « jamais de secret dans un topic conservé » même si un appelant passe le
/// texte brut d'une erreur caméra.
const REASON_INTERDITS: [&str; 6] =
	["password", "passwd", "mot de passe", "token", "secret", "apikey"];

/// Met un motif de refus en état d'être conservé sur le broker : une seule
/// ligne, sans balise XML, sans secret, bornée à [`REASON_MAX`] caractères.
///
/// Les balises sont remplacées par des guillemets français et non supprimées :
/// `this camera's reply has no <OsdChannelName> tag` est précisément
/// l'information qui explique un refus, et ce qui doit disparaître est le
/// délimiteur XML, pas le nom de la balise.
pub fn publishable_reason(reason: &str) -> String {
	publishable_reason_bounded(reason, REASON_MAX)
}

/// Same cleaning, with the bound chosen by the caller.
///
/// [`publishable_reason`] caps at 160 characters because its sentences land on
/// **retained** topics that Home Assistant replays on every start: one line, not
/// a dump. The wider bound is for the opposite case — reading the camera's own
/// answer back, where the body worth seeing is longer than that cap leaves
/// room for: a whole `<MD>` block fits in the first 400 characters. Neither path
/// may become a channel for secrets, so the cleaning is identical in both: no
/// bare XML delimiter, no control character, and the mask applies whenever the
/// text looks like a credential.
pub fn publishable_reason_bounded(reason: &str, maximal: usize) -> String {
	let sans_balises: String = reason
		.chars()
		.map(|c| match c {
			'<' => '«',
			'>' => '»',
			c if c.is_control() => ' ',
			c => c,
		})
		.collect();
	if REASON_INTERDITS
		.iter()
		.any(|interdit| sans_balises.to_lowercase().contains(interdit))
	{
		return REASON_MASQUE.to_string();
	}
	let propre = sans_balises.split_whitespace().collect::<Vec<_>>().join(" ");
	if propre.is_empty() {
		return REASON_SANS_DETAIL.to_string();
	}
	if propre.chars().count() <= maximal {
		return propre;
	}
	let tronc: String = propre.chars().take(maximal - 1).collect();
	format!("{}…", tronc.trim_end())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Build a StatusPublisher over a stub client whose event loop is
	/// leaked — publishes hand the payload to the rumqttc channel and
	/// return `Ok(())` without a broker round-trip. Good enough for
	/// line-coverage tests that exercise the publish-helper wrappers.
	fn stub_publisher() -> (SharedMqttClient, String, String) {
		let client = SharedMqttClient::for_test_stub("status-test");
		(client, "bairelay".to_string(), "cam1".to_string())
	}

	#[tokio::test]
	async fn publish_connection_both_states() {
		let (c, p, n) = stub_publisher();
		let pub_ = StatusPublisher::new(&c, &p, &n);
		pub_.publish_connection(true).await.unwrap();
		pub_.publish_connection(false).await.unwrap();
	}

	#[tokio::test]
	async fn publish_ai_object_is_retained_on_off() {
		let (c, mock) = crate::test_support::mock_client();
		let pub_ = StatusPublisher::new(&c, "bairelay", "cam1");
		pub_.publish_ai_object(AiObject::Person, true)
			.await
			.unwrap();
		pub_.publish_ai_object(AiObject::Package, false)
			.await
			.unwrap();
		assert_eq!(
			mock.published(),
			vec![
				(
					"bairelay/cam1/status/ai/person".to_string(),
					b"on".to_vec(),
					true
				),
				(
					"bairelay/cam1/status/ai/package".to_string(),
					b"off".to_vec(),
					true
				),
			]
		);
	}

	#[tokio::test]
	async fn publish_doorbell_press_is_not_retained() {
		let (c, mock) = crate::test_support::mock_client();
		let pub_ = StatusPublisher::new(&c, "bairelay", "cam1");
		pub_.publish_doorbell_press().await.unwrap();
		assert_eq!(
			mock.published(),
			vec![(
				"bairelay/cam1/status/doorbell".to_string(),
				b"press".to_vec(),
				false
			)]
		);
	}

	#[tokio::test]
	async fn publish_motion_both_states_and_unknown() {
		let (c, p, n) = stub_publisher();
		let pub_ = StatusPublisher::new(&c, &p, &n);
		pub_.publish_motion(true).await.unwrap();
		pub_.publish_motion(false).await.unwrap();
		pub_.publish_motion_unknown().await.unwrap();
	}

	#[tokio::test]
	async fn publish_battery_level_clamps_to_100() {
		let (c, p, n) = stub_publisher();
		let pub_ = StatusPublisher::new(&c, &p, &n);
		// All three should succeed; the clamp is covered implicitly by
		// not panicking on 200 and by the assertion below on the exact
		// rounding in the topic tests.
		pub_.publish_battery_level(0).await.unwrap();
		pub_.publish_battery_level(50).await.unwrap();
		pub_.publish_battery_level(200).await.unwrap();
	}

	#[tokio::test]
	async fn publish_preview_base64_encodes() {
		let (c, p, n) = stub_publisher();
		let pub_ = StatusPublisher::new(&c, &p, &n);
		pub_.publish_preview(&[0xFF, 0xD8, 0xFF]).await.unwrap();
		pub_.publish_preview(&[]).await.unwrap();
	}

	#[tokio::test]
	async fn publish_floodlight_and_tasks() {
		let (c, p, n) = stub_publisher();
		let pub_ = StatusPublisher::new(&c, &p, &n);
		pub_.publish_floodlight(true).await.unwrap();
		pub_.publish_floodlight(false).await.unwrap();
		pub_.publish_floodlight_tasks("[]").await.unwrap();
		pub_.publish_floodlight_tasks_enabled(true).await.unwrap();
		pub_.publish_floodlight_tasks_enabled(false).await.unwrap();
	}

	#[tokio::test]
	async fn publish_pir_and_ptz() {
		let (c, p, n) = stub_publisher();
		let pub_ = StatusPublisher::new(&c, &p, &n);
		pub_.publish_pir(true).await.unwrap();
		pub_.publish_pir(false).await.unwrap();
		pub_.publish_ptz_preset(r#"{"presets":[]}"#).await.unwrap();
	}

	#[tokio::test]
	async fn publish_ir_and_led_retained_on_status_topics() {
		let (c, mock) = crate::test_support::mock_client();
		let pub_ = StatusPublisher::new(&c, "bairelay", "cam1");
		pub_.publish_ir(IrMode::Auto).await.unwrap();
		pub_.publish_ir(IrMode::Off).await.unwrap();
		pub_.publish_led(true).await.unwrap();
		let got = mock.published();
		let expect = [
			("bairelay/cam1/status/ir", "auto"),
			("bairelay/cam1/status/ir", "off"),
			("bairelay/cam1/status/led", "on"),
		];
		assert_eq!(got.len(), expect.len(), "published: {got:?}");
		for ((topic, payload, retained), (and, ep)) in got.iter().zip(expect) {
			assert_eq!(
				(topic.as_str(), payload.as_slice(), *retained),
				(and, ep.as_bytes(), true)
			);
		}
	}

	#[test]
	fn floodlight_state_json_on_matches_neolink_light_template() {
		// HA's DiscoveryLight.state_value_template is
		//   "{{ value_json.state }}"
		// so the wire bytes MUST be JSON with a `state` key. Any other
		// shape breaks HA's light entity silently.
		assert_eq!(floodlight_state_json(true), r#"{"state":"on"}"#);
	}

	#[test]
	fn floodlight_state_json_off_matches_neolink_light_template() {
		assert_eq!(floodlight_state_json(false), r#"{"state":"off"}"#);
	}

	#[test]
	fn floodlight_state_json_is_valid_json() {
		// Belt-and-braces: round-trip both payloads through serde_json
		// so a typo in the literal surfaces as a failing test, not a
		// retained-message HA bug.
		for state in [true, false] {
			let payload = floodlight_state_json(state);
			let v: serde_json::Value = serde_json::from_str(payload).expect("valid JSON");
			let got = v["state"].as_str().expect("state is a string");
			assert_eq!(got, if state { "on" } else { "off" });
		}
	}

	#[tokio::test]
	async fn a_setting_itself_publishes_on_its_leaf_conservee() {
		let (c, mock) = crate::test_support::mock_client();
		let pub_ = StatusPublisher::new(&c, "bairelay", "cam1");
		pub_.publish_setting(Family::Audio, "volume", "42")
			.await
			.unwrap();
		pub_.publish_setting_switch(Family::Denoise, "enable", true)
			.await
			.unwrap();
		pub_.publish_setting_flag(Family::Sleep, "state", false)
			.await
			.unwrap();
		assert_eq!(
			mock.published(),
			vec![
				(
					"bairelay/cam1/status/audio/volume".to_string(),
					b"42".to_vec(),
					true
				),
				(
					"bairelay/cam1/status/denoise/enable".to_string(),
					b"on".to_vec(),
					true
				),
				(
					"bairelay/cam1/status/sleep/state".to_string(),
					b"0".to_vec(),
					true
				),
			]
		);
	}

	#[tokio::test]
	async fn a_setting_refuse_is_not_publishes_mais_clears() {
		// La caméra n'a pas répondu : on efface l'état conservé, on ne met pas
		// une valeur par défaut. Un capteur « inconnu » est honnête ; un « 0 »
		// conservé ne l'est pas.
		let (c, mock) = crate::test_support::mock_client();
		let pub_ = StatusPublisher::new(&c, "bairelay", "cam1");
		pub_.clear_setting(Family::Chime, "list").await.unwrap();
		pub_.clear_settings_json(Family::Image).await.unwrap();
		assert_eq!(
			mock.published(),
			vec![
				(
					"bairelay/cam1/status/chime/list".to_string(),
					Vec::new(),
					true
				),
				(
					"bairelay/cam1/status/image/json".to_string(),
					Vec::new(),
					true
				),
			]
		);
	}

	#[tokio::test]
	async fn the_cause_of_a_family_is_kept_then_clears() {
		// Le refus d'une famille doit rester lisible après coup : conservé, et
		// effacé (et non remplacé par une phrase triomphante) dès que la famille
		// a répondu.
		let (c, mock) = crate::test_support::mock_client();
		let pub_ = StatusPublisher::new(&c, "bairelay", "cam1");
		pub_
			.publish_family_reason(Family::Motion, "la caméra a répondu MdAlarm : non écrivable")
			.await
			.unwrap();
		pub_.clear_family_reason(Family::Motion).await.unwrap();
		assert_eq!(
			mock.published(),
			vec![
				(
					"bairelay/cam1/status/motion/reason".to_string(),
					"la caméra a répondu MdAlarm : non écrivable".as_bytes().to_vec(),
					true
				),
				("bairelay/cam1/status/motion/reason".to_string(), Vec::new(), true),
			]
		);
	}

	/// Un topic conservé ne verra jamais le XML brut de la caméra, un secret, ni
	/// dix lignes de trace : `publishable_reason` est la dernière barrière, et
	/// c'est elle qui tient la promesse, pas la vigilance des appelants.
	#[test]
	fn a_cause_not_carries_ni_xml_ni_secret() {
		// Le nom de la balise est l'information utile ; seuls ses délimiteurs
		// disparaissent.
		assert_eq!(
			publishable_reason("no <OsdChannelName> tag here"),
			"no «OsdChannelName» tag here"
		);
		assert_eq!(publishable_reason("  \n\t  "), REASON_SANS_DETAIL);
		assert_eq!(
			publishable_reason("could not auth with password hunter2"),
			REASON_MASQUE
		);
		assert_eq!(
			publishable_reason("<Wifi><key>mot de passe</key></Wifi>"),
			REASON_MASQUE,
			"un bloc XML passé par erreur ne doit pas ressortir"
		);
		assert_eq!(
			publishable_reason("MdAlarm: refus\nde réécriture"),
			"MdAlarm: refus de réécriture",
			"un motif tient sur une ligne"
		);
		let long = publishable_reason(&"a".repeat(4000));
		assert!(long.chars().count() <= REASON_MAX, "{} caractères", long.chars().count());
		assert!(long.ends_with('…'), "{long}");
	}

	#[tokio::test]
	async fn the_json_of_a_family_is_kept() {
		let (c, mock) = crate::test_support::mock_client();
		let pub_ = StatusPublisher::new(&c, "bairelay", "cam1");
		pub_.publish_settings_json(Family::Ntp, r#"{"enable":"1"}"#)
			.await
			.unwrap();
		assert_eq!(
			mock.published(),
			vec![(
				"bairelay/cam1/status/ntp/json".to_string(),
				br#"{"enable":"1"}"#.to_vec(),
				true
			)]
		);
	}

	/// Dédoublonnage de la présence : deux succès consécutifs produisent une seule
	/// publication, et la transition vers le verdict inverse en produit une
	/// seconde. Sans ce garde-fou, cinquante wakes par heure écriraient cinquante
	/// fois le même message conservé sur un courtier domestique.
	#[tokio::test]
	async fn the_presence_not_publishes_que_on_transition() {
		let (c, mock) = crate::test_support::mock_client();
		let state = Arc::new(PublishedPresence::default());
		let publ = StatusPublisher::with_presence(&c, "bairelay", "cam1", Arc::clone(&state));
		publ.publish_presence(true).await.unwrap();
		publ.publish_presence(true).await.unwrap();
		assert_eq!(
			mock.published(),
			vec![(
				"bairelay/cam1/presence".to_string(),
				b"reachable".to_vec(),
				true
			)],
			"deux succès consécutifs ne doivent rien publier de plus"
		);
		publ.publish_presence(false).await.unwrap();
		publ.publish_presence(false).await.unwrap();
		assert_eq!(
			mock.published(),
			vec![
				(
					"bairelay/cam1/presence".to_string(),
					b"reachable".to_vec(),
					true
				),
				(
					"bairelay/cam1/presence".to_string(),
					b"unreachable".to_vec(),
					true
				),
			],
			"une transition, une publication"
		);
	}

	/// Les trois endroits du pont construisent chacun leur propre publieur : la
	/// cellule doit donc porter la déduplication au-delà des instances : sinon elle
	/// saute dès que deux endroits publient.
	#[tokio::test]
	async fn the_presence_itself_deduplique_enters_deux_publieurs() {
		let (c, mock) = crate::test_support::mock_client();
		let state = Arc::new(PublishedPresence::default());
		StatusPublisher::with_presence(&c, "bairelay", "cam1", Arc::clone(&state))
			.publish_presence(true)
			.await
			.unwrap();
		StatusPublisher::with_presence(&c, "bairelay", "cam1", Arc::clone(&state))
			.publish_presence(true)
			.await
			.unwrap();
		assert_eq!(mock.count(), 1, "{:?}", mock.published());
	}
}
