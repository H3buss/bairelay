//! Entités de réglages pour la discovery Home Assistant.
//!
//! Tout le cahier des charges des réglages vit dans
//! `docs/protocole-reglages.md`; ce fichier n'en est que la vitrine MQTT. Il
//! est **tableau-driven** pour une raison : chaque entité décrite ici doit
//! tomber pile sur les topics que le binaire publie (`status/<famille>/<feuille>`)
//! et accepte (`control/<famille>/<feuille>`). Une liste de spécifications
//! unique, lue à la fois par l'émetteur de discovery et par les tests, est le
//! seul moyen de garantir que les deux côtés ne dérivent pas l'un de l'autre.
//!
//! Deux plateformes HA absentes de la version d'origine sont ajoutées ici :
//!
//! * `number` — un réglage numérique borné. Les bornes viennent **du doc du
//!   protocole**, jamais d'un goût : 0-100 pour les volumes, 1-100 pour le
//!   débruiteur, 0-255 pour les curseurs image, 0-50 pour la sensibilité de
//!   détection. Une borne inconnue n'est pas inventée : le réglage est alors
//!   exposé en lecture seule, ou pas exposé du tout.
//! * `text` — une chaîne (serveur NTP, noms). Bornée en longueur, parce que
//!   le firmware ne dit pas ce qu'il accepte au-delà de ce qu'il imprime.

use std::collections::HashSet;

use serde::Serialize;

use crate::settings::Family;

use super::{
	DiscoveryAvailability, DiscoveryContext, DiscoveryDevice, Feature, availability_blocks,
	cam_topic, config_topic, device_block, to_json, unique,
};

/// Une entité `number` HA : `command_topic` + `state_topic` bornés.
#[derive(Serialize, Debug)]
pub struct DiscoveryNumber {
	pub name: String,
	pub unique_id: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub icon: Option<String>,
	pub device: DiscoveryDevice,
	pub availability: Vec<DiscoveryAvailability>,
	pub command_topic: String,
	pub state_topic: String,
	pub min: f64,
	pub max: f64,
	pub step: f64,
	/// `"slider"` (curseur) ou `"box"` (saisie) — HA exige l'un des deux.
	pub mode: &'static str,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub unit_of_measurement: Option<&'static str>,
}

/// Une entité `text` HA : une chaîne bornée en longueur.
#[derive(Serialize, Debug)]
pub struct DiscoveryText {
	pub name: String,
	pub unique_id: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub icon: Option<String>,
	pub device: DiscoveryDevice,
	pub availability: Vec<DiscoveryAvailability>,
	pub command_topic: String,
	pub state_topic: String,
	/// Longueur minimale acceptée (0 = effacement autorisé).
	pub min: usize,
	/// Longueur maximale acceptée, celle que le pont valide avant d'écrire.
	pub max: usize,
	/// `"text"` (lisible) — jamais `"password"` : aucun réglage de ce pont
	/// n'est un secret, et ce qui pourrait l'être (`<Wifi><key>`) n'est pas
	/// publié du tout.
	pub mode: &'static str,
}

/// Un capteur d'état dont l'unité peut être absente.
///
/// `DiscoverySensor` (le struct historique, pour la battery) impose une
/// unité. Plusieurs états de réglages n'en ont **aucune** de documentée — le
/// `<signal>` du Wi-Fi est un nombre dont personne ne sait s'il est en dBm ou
/// en pourcents. Leur rendre une unité serait un mensonge affiché dans HA,
/// donc l'unité est optionnelle ici.
#[derive(Serialize, Debug)]
pub struct DiscoveryStateSensor {
	pub name: String,
	pub unique_id: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub icon: Option<String>,
	pub device: DiscoveryDevice,
	pub availability: Vec<DiscoveryAvailability>,
	pub state_topic: String,
	/// `measurement` pour un nombre, `None` pour un état textuel. Home
	/// Assistant refuse un `state_class` numérique sur un état qui ne l'est
	/// pas : le publier sur une langue ou une liste de carillons serait remplacer
	/// un silence par un avertissement répdst dans le journal.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub state_class: Option<&'static str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub unit_of_measurement: Option<&'static str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub device_class: Option<&'static str>,
}

/// La plateforme HA et ses réglages pour une spécification d'entité.
#[derive(Debug, Clone, Copy)]
pub enum EntityKind {
	/// Réglage numérique borné (plateforme `number`).
	Number {
		min: f64,
		max: f64,
		step: f64,
		unit: Option<&'static str>,
		/// `true` = curseur, `false` = boîte de saisie.
		slider: bool,
	},
	/// Chaîne bornée (plateforme `text`).
	Text { max_len: usize },
	/// `switch` on/off, état relu en `"on"`/`"off"`.
	Switch,
	/// Le jumeau **lisible** d'un `switch` : `binary_sensor` sur les mêmes
	/// textes `"on"`/`"off"`, sans topic de commande. Ce n'est pas un
	/// `FlagSensor` : celui-ci lit le `"1"`/`"0"` brut du protocole, alors
	/// qu'un interrupteur publie le texte que l'entité `switch` attend.
	OnOffSensor,
	/// `select` à options fermées, exactement les textes que la caméra accepte.
	Select(&'static [&'static str]),
	/// `binary_sensor` en lecture seule sur un `"1"`/`"0"` caméra.
	FlagSensor {
		device_class: Option<&'static str>,
	},
	/// `sensor` numérique ou texte, en lecture seule.
	StateSensor {
		unit: Option<&'static str>,
		device_class: Option<&'static str>,
		/// `measurement` (se conserve dans l'historique) ou `None`.
		class: Option<&'static str>,
	},
	/// `button` : un appui, un message MQTT.
	Button { payload: &'static str },
}

/// Une entité de réglage : la famille/feuille de topic, le libellé HA, et la
/// plateforme. `family`/`leaf` **doivent** correspondre à
/// [`crate::settings::Family`] / [`crate::settings::SettingCommand`] côté
/// parsing, et aux topics que le dispatch publie après relecture.
#[derive(Debug, Clone, Copy)]
pub struct EntitySpec {
	pub feature: Feature,
	pub family: &'static str,
	pub leaf: &'static str,
	/// Libellé collé après le nom de la caméra ("Porche Volume interphone").
	pub label: &'static str,
	pub icon: Option<&'static str>,
	pub kind: EntityKind,
}

/// Ce qui, parmi les genres, n'a **jamais** de topic de commande. Une entité
/// construite sur l'un de ces genres ne peut pas être actionnée depuis Home
/// Assistant : c'est la forme sous laquelle une famille de réglages est publiée
/// tant que la caméra n'a pas prouvé qu'elle la relit et l'accepte en retour.
pub fn is_read_only(kind: &EntityKind) -> bool {
	matches!(
		kind,
		EntityKind::FlagSensor { .. } | EntityKind::OnOffSensor | EntityKind::StateSensor { .. }
	)
}

impl EntitySpec {
	/// La plateforme HA (`switch`, `number`, …) et le segment de topic
	/// `homeassistant/<plateforme>/...`. C'est le **genre construit** qui décide,
	/// pas celui écrit au tableau : la copie lisible d'un `number` est un
	/// `sensor`, donc son topic de configuration change de segment — d'où
	/// l'obligation, chez l'opératrice, de supprimer l'entité que Home Assistant
	/// garde en mémoire (voir `docs/lot-reglages-mqtt.md`).
	fn ha_platform(kind: &EntityKind) -> &'static str {
		match kind {
			EntityKind::Number { .. } => "number",
			EntityKind::Text { .. } => "text",
			EntityKind::Switch => "switch",
			EntityKind::Select(_) => "select",
			EntityKind::FlagSensor { .. } | EntityKind::OnOffSensor => "binary_sensor",
			EntityKind::StateSensor { .. } => "sensor",
			EntityKind::Button { .. } => "button",
		}
	}

	/// L'identifiant unique HA : `{prefix}_{cam}_{family}_{leaf}` — la
	/// famille est dans l' id parce que plusieurs familles ont une feuille
	/// `enable` ou `sensitivity`.
	pub fn unique_id(&self, ctx: &DiscoveryContext) -> String {
		unique(ctx.topic_prefix, ctx.camera_name, &format!("{}_{}", self.family, self.leaf))
	}

	/// Le topic d'état lu par l'entité.
	pub fn state_topic(&self, ctx: &DiscoveryContext) -> String {
		cam_topic(ctx, "status", &format!("{}/{}", self.family, self.leaf))
	}

	/// Le topic de commande, absent pour ce qui est en lecture seule. Le genre
	/// construit décide, pas le genre du tableau : c'est ce qui fait qu'une
	/// famille non relue se publie sans aucune commande.
	pub fn command_topic(&self, ctx: &DiscoveryContext, kind: &EntityKind) -> Option<String> {
		if is_read_only(kind) {
			return None;
		}
		Some(cam_topic(ctx, "control", &format!("{}/{}", self.family, self.leaf)))
	}

	/// Construit `(topic_de_config, payload)` du genre écrit au tableau : la
	/// forme **modifiable** de l'entité.
	pub fn build(&self, ctx: &DiscoveryContext, friendly: &str) -> (String, Vec<u8>) {
		self.build_kind(ctx, friendly, &self.kind)
	}

	/// Construit l'entité telle que la famille doit être publiée **aujourd'hui** :
	/// modifiable si la caméra a répondu à une lecture de cette famille et
	/// accepte qu'on lui renvoie le bloc (`ecrivable`), sinon sa copie en lecture
	/// seule — même `unique_id`, même topic d'état, plateforme différente, aucun
	/// `command_topic`.
	///
	/// `None` = aucune entité du tout. Un bouton n'a pas d'équivalent lisible :
	/// il n'y a rien à afficher, seulement quelque chose à déclencher. Le publier
	/// avant que la caméra ait confirmé le carillon, ce serait promettre un bouton
	/// qui ne sonne pas — exactement le reproche fait à la version d'origine.
	pub fn build_from(
		&self,
		ctx: &DiscoveryContext,
		friendly: &str,
		writable: bool,
	) -> Option<(String, Vec<u8>)> {
		if writable {
			return Some(self.build(ctx, friendly));
		}
		let twin = self.read_only_twin()?;
		Some(self.build_kind(ctx, friendly, &twin))
	}

	/// Le genre qui affiche la même feuille sans offrir de commande. Les bornes
	/// d'un `number` n'y figurent pas : ce sont des bornes d'**écriture**, et une
	/// entité qui ne peut pas écrire n'a rien à en faire. L'unité, elle, reste :
	/// elle dit ce que le nombre lu veut dire.
	fn read_only_twin(&self) -> Option<EntityKind> {
		Some(match self.kind {
			EntityKind::Number { unit, .. } => {
				EntityKind::StateSensor { unit, device_class: None, class: Some("measurement") }
			}
			// Un texte et un choix sont des états parmi d'autres : ni unité, ni
			// `state_class` (Home Assistant refuse du non numérique mesuré).
			EntityKind::Text { .. } | EntityKind::Select(_) => {
				EntityKind::StateSensor { unit: None, device_class: None, class: None }
			}
			EntityKind::Switch => EntityKind::OnOffSensor,
			EntityKind::Button { .. } => return None,
			lu @ (EntityKind::FlagSensor { .. }
			| EntityKind::OnOffSensor
			| EntityKind::StateSensor { .. }) => lu,
		})
	}

	/// `(topic_de_config, payload)` pour un genre donné — celui du tableau ou son
	/// jumeau lisible. Les deux partagent le même code pour qu'une copie en
	/// lecture seule ne puisse pas oublier un bloc (device, disponibilité).
	fn build_kind(
		&self,
		ctx: &DiscoveryContext,
		friendly: &str,
		kind: &EntityKind,
	) -> (String, Vec<u8>) {
		let unique_id = self.unique_id(ctx);
		// Le libellé dit **ce que c'est**, jamais à qui c'est : Home Assistant
		// affiche déjà l'appareil au-dessus de l'entité, et répéter le nom de la
		// caméra (« Sonnette Volume général ») tronque le libellé utile pour ne
		// rien ajouter. `unique_id` et le bloc `device` gardent le nom de la
		// caméra : ce sont les identifiants, eux ne sont pas renommés.
		let name = self.label.to_string();
		let device = device_block(ctx, friendly);
		let availability = availability_blocks(ctx);
		let state_topic = self.state_topic(ctx);
		let command_topic = self.command_topic(ctx, kind);
		let payload = match kind {
			EntityKind::Number {
				min,
				max,
				step,
				unit,
				slider,
			} => to_json(&DiscoveryNumber {
				name,
				unique_id: unique_id.clone(),
				icon: self.icon.map(str::to_string),
				device,
				availability,
				command_topic: command_topic.expect("un number a un command_topic"),
				state_topic,
				min: *min,
				max: *max,
				step: *step,
				mode: if *slider { "slider" } else { "box" },
				unit_of_measurement: *unit,
			}),
			EntityKind::Text { max_len } => to_json(&DiscoveryText {
				name,
				unique_id: unique_id.clone(),
				icon: self.icon.map(str::to_string),
				device,
				availability,
				command_topic: command_topic.expect("un text a un command_topic"),
				state_topic,
				min: 0,
				max: *max_len,
				mode: "text",
			}),
			EntityKind::Switch => to_json(&super::DiscoverySwitch {
				name,
				unique_id: unique_id.clone(),
				icon: self.icon.map(str::to_string),
				device,
				availability,
				command_topic: command_topic.expect("un switch a un command_topic"),
				payload_on: "on".to_string(),
				payload_off: "off".to_string(),
				state_topic: Some(state_topic),
				state_on: Some("on".to_string()),
				state_off: Some("off".to_string()),
			}),
			EntityKind::OnOffSensor => to_json(&super::DiscoveryBinarySensor {
				name,
				unique_id: unique_id.clone(),
				device_class: None,
				icon: self.icon.map(str::to_string),
				device,
				availability,
				// Les deux textes que `put_switch` publie après relecture.
				payload_on: "on".to_string(),
				payload_off: "off".to_string(),
				state_topic,
			}),
			EntityKind::Select(options) => to_json(&super::DiscoverySelect {
				name,
				unique_id: unique_id.clone(),
				icon: self.icon.map(str::to_string),
				device,
				availability,
				command_topic: command_topic.expect("un select a un command_topic"),
				options: options.iter().map(|s| s.to_string()).collect(),
				state_topic: Some(state_topic),
			}),
			EntityKind::FlagSensor { device_class } => to_json(&super::DiscoveryBinarySensor {
				name,
				unique_id: unique_id.clone(),
				device_class: device_class.map(str::to_string),
				icon: self.icon.map(str::to_string),
				device,
				availability,
				payload_on: "1".to_string(),
				payload_off: "0".to_string(),
				state_topic,
			}),
			EntityKind::StateSensor {
				unit,
				device_class,
				class,
			} => to_json(&DiscoveryStateSensor {
				name,
				unique_id: unique_id.clone(),
				icon: self.icon.map(str::to_string),
				device,
				availability,
				state_topic,
				// `class` est la décision de la ligne, pas une valeur par défaut :
				// une ligne textuelle (liste JSON, état de charge, mélodie) qui
				// se prétendrait `measurement` verrait Home Assistant refuser
				// ses états non numériques — l'entité resterait « inconnue ».
				state_class: *class,
				unit_of_measurement: *unit,
				device_class: *device_class,
			}),
			EntityKind::Button { payload } => to_json(&super::DiscoveryButton {
				name,
				unique_id: unique_id.clone(),
				icon: self.icon.map(str::to_string),
				device,
				availability,
				command_topic: command_topic.expect("un button a un command_topic"),
				payload_press: Some((*payload).to_string()),
			}),
		};
		(
			config_topic(ctx, Self::ha_platform(kind), &unique_id),
			payload,
		)
	}

	/// La plateforme d'une copie en lecture seule (`sensor` ou `binary_sensor`),
	/// pour les tests et le journal. `None` = aucune entité lisible : seul le
	/// bouton, qui n'a pas d'état, disparaît complètement tant que la famille
	/// n'est pas relue.
	pub fn read_only_platform(&self) -> Option<&'static str> {
		Some(Self::ha_platform(&self.read_only_twin()?))
	}
}

/// Les jours acceptés par `<AutoReboot><weekDay>`, dans l'ordre d'affichage.
pub const WEEKDAYS: [&str; 8] = [
	"everyday", "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

/// Les formats de date OSD.
pub const OSD_FORMATS: [&str; 3] = ["DMY", "MDY", "YMD"];

/// Les modes jour/nuit normalisés par la lib de référence.
pub const DAY_NIGHT_MODES: [&str; 3] = ["auto", "color", "blackAndWhite"];

/// Toutes les entités de réglages, groupées par famille de topic.
///
/// L'ordre est stable (les tests comparent des compteurs et des listes de
/// topics). Chaque ligne porte la plage **du protocole** : modifier une borne
/// sans capture locale derrière soi, c'est écrire une valeur que la caméra
/// rejettera en silence.
pub const SPECS: &[EntitySpec] = &[
	// ── audioCfg (264 lecture / 265 écriture) ───────────────────────────
	//
	// Le bloc imprime **trois** volumes, et chacun règle un haut-parleur
	// différent. La version d'origine n'en découvrait que deux, sous un libellé
	// générique : « Volume interphone » écrivait `<talkAndReplyVolume>` sans le
	// dire, et `<volume>` — le volume général du haut-parleur de la caméra —
	// n'avait aucune entité. Chaque ligne nomme donc la balise XML qu'elle
	// porte : c'est la seule chose qui se vérifie contre une capture.
	EntitySpec {
		feature: Feature::Audio,
		family: "audio",
		// `<volume>`. Identifiant **nouveau** (`bairelay_<cam>_audio_general_volume`) :
		// `..._audio_volume` et `..._audio_visitor_volume` existent déjà chez
		// l'opératrice et ne sont ni renommés ni recyclés.
		leaf: "general_volume",
		label: "Speaker volume",
		icon: Some("mdi:volume-high"),
		kind: EntityKind::Number { min: 0.0, max: 100.0, step: 1.0, unit: Some("%"), slider: true },
	},
	EntitySpec {
		feature: Feature::Audio,
		family: "audio",
		// `<talkAndReplyVolume>` : ce que le visiteur entend pendant l'échange.
		leaf: "volume",
		label: "Intercom volume (talk)",
		icon: Some("mdi:walkie-talkie"),
		kind: EntityKind::Number { min: 0.0, max: 100.0, step: 1.0, unit: Some("%"), slider: true },
	},
	EntitySpec {
		feature: Feature::Audio,
		family: "audio",
		// `<visitorVolume>` : ce que la caméra envoie dans le talkie-walkie.
		leaf: "visitor_volume",
		label: "Visitor volume",
		icon: Some("mdi:volume-heart"),
		kind: EntityKind::Number { min: 0.0, max: 100.0, step: 1.0, unit: Some("%"), slider: true },
	},
	EntitySpec {
		feature: Feature::Audio,
		family: "audio",
		leaf: "visitor_loudspeaker",
		label: "Visitor loudspeaker",
		icon: Some("mdi:speaker"),
		kind: EntityKind::Switch,
	},
	// ── aiDenoise (439 / 440) ───────────────────────────────────────────
	EntitySpec {
		feature: Feature::Denoise,
		family: "denoise",
		leaf: "enable",
		label: "Microphone denoise",
		icon: Some("mdi:noise-reduction"),
		kind: EntityKind::Switch,
	},
	EntitySpec {
		feature: Feature::Denoise,
		family: "denoise",
		leaf: "level",
		label: "Force denoise",
		icon: Some("mdi:tune-variant"),
		kind: EntityKind::Number { min: 1.0, max: 100.0, step: 1.0, unit: Some("%"), slider: true },
	},
	// ── Image (26 / 25) : curseurs 0-255 ────────────────────────────────
	EntitySpec {
		feature: Feature::Image,
		family: "image",
		leaf: "bright",
		label: "Brightness",
		icon: Some("mdi:brightness-7"),
		kind: EntityKind::Number { min: 0.0, max: 255.0, step: 1.0, unit: None, slider: true },
	},
	EntitySpec {
		feature: Feature::Image,
		family: "image",
		leaf: "contrast",
		label: "Contrast",
		icon: Some("mdi:contrast-box"),
		kind: EntityKind::Number { min: 0.0, max: 255.0, step: 1.0, unit: None, slider: true },
	},
	EntitySpec {
		feature: Feature::Image,
		family: "image",
		leaf: "saturation",
		label: "Saturation",
		icon: Some("mdi:water-opacity"),
		kind: EntityKind::Number { min: 0.0, max: 255.0, step: 1.0, unit: None, slider: true },
	},
	EntitySpec {
		feature: Feature::Image,
		family: "image",
		leaf: "hue",
		label: "Hue",
		icon: Some("mdi:palette"),
		kind: EntityKind::Number { min: 0.0, max: 255.0, step: 1.0, unit: None, slider: true },
	},
	EntitySpec {
		feature: Feature::Image,
		family: "image",
		leaf: "sharpen",
		label: "Sharpness",
		icon: Some("mdi:image-filter-center-focus"),
		kind: EntityKind::Number { min: 0.0, max: 255.0, step: 1.0, unit: None, slider: true },
	},
	EntitySpec {
		feature: Feature::Image,
		family: "image",
		leaf: "day_night",
		label: "Day/night mode",
		icon: Some("mdi:weather-night"),
		kind: EntityKind::Select(&DAY_NIGHT_MODES),
	},
	// ── OSD (44 / 45) ───────────────────────────────────────────────────
	EntitySpec {
		feature: Feature::Osd,
		family: "osd",
		leaf: "datetime_enable",
		label: "On-screen timestamp",
		icon: Some("mdi:clock-outline"),
		kind: EntityKind::Switch,
	},
	EntitySpec {
		feature: Feature::Osd,
		family: "osd",
		leaf: "name_enable",
		label: "On-screen name",
		icon: Some("mdi:signature-text"),
		kind: EntityKind::Switch,
	},
	EntitySpec {
		feature: Feature::Osd,
		family: "osd",
		leaf: "name",
		label: "On-screen name text",
		icon: Some("mdi:account-box"),
		kind: EntityKind::Text { max_len: 31 },
	},
	EntitySpec {
		feature: Feature::Osd,
		family: "osd",
		leaf: "watermark",
		label: "Watermark",
		icon: Some("mdi:watermark"),
		kind: EntityKind::Switch,
	},
	// ── Détection de mouvement (46 / 47) ────────────────────────────────
	EntitySpec {
		feature: Feature::MotionSettings,
		family: "motion",
		leaf: "enable",
		label: "Motion detection",
		icon: Some("mdi:run-fast"),
		kind: EntityKind::Switch,
	},
	EntitySpec {
		feature: Feature::MotionSettings,
		family: "motion",
		leaf: "sensitivity",
		label: "Motion sensitivity",
		icon: Some("mdi:speedometer"),
		// 0-50, plus haut = plus sensible (doc §4.1).
		kind: EntityKind::Number { min: 0.0, max: 50.0, step: 1.0, unit: None, slider: true },
	},
	// ── PIR (212 / 213) : l'interrupteur vient de `build_pir` ───────────
	EntitySpec {
		feature: Feature::Pir,
		family: "pir",
		leaf: "sensitivity",
		label: "PIR sensitivity",
		icon: Some("mdi:sensor-outline"),
		// `sensiValue` n'a pas de plage documentée ; 0-100 est la borne
		// utilisée par le client de référence, et la caméra garde ce
		// qu'elle accepte.
		kind: EntityKind::Number { min: 0.0, max: 100.0, step: 1.0, unit: None, slider: true },
	},
	EntitySpec {
		feature: Feature::Pir,
		family: "pir",
		leaf: "interval",
		label: "PIR trigger interval",
		icon: Some("mdi:timer-sand"),
		// `<interval>` du même `<RfAlarmCfg>` (212/213) : le temps de repos que
		// la caméra s'impose entre deux alarmes PIR, **en secondes**. La plage
		// 5-120 est celle que l'application Reolink expose pour ce réglage ; le
		// firmware annonce ses propres bornes dans `intervalSecMin`/`intervalSecMax`,
		// visibles dans `status/pir/json`. Une valeur hors de la plage ne part pas.
		kind: EntityKind::Number { min: 5.0, max: 120.0, step: 1.0, unit: Some("s"), slider: true },
	},
	// ── NTP (38 / 39) ───────────────────────────────────────────────────
	EntitySpec {
		feature: Feature::Ntp,
		family: "ntp",
		leaf: "enable",
		label: "NTP sync",
		icon: Some("mdi:clock-check"),
		kind: EntityKind::Switch,
	},
	EntitySpec {
		feature: Feature::Ntp,
		family: "ntp",
		leaf: "server",
		label: "NTP server",
		icon: Some("mdi:server-network"),
		kind: EntityKind::Text { max_len: 63 },
	},
	EntitySpec {
		feature: Feature::Ntp,
		family: "ntp",
		leaf: "interval",
		label: "Sync interval",
		icon: Some("mdi:restore"),
		// Minutes ; 1440 = le défaut du client de référence (24 h).
		kind: EntityKind::Number {
			min: 1.0,
			max: 1440.0,
			step: 1.0,
			unit: Some("min"),
			slider: false,
		},
	},
	// ── Heure d'dst (106 / 107) ─────────────────────────────────────────
	EntitySpec {
		feature: Feature::Dst,
		family: "dst",
		leaf: "enable",
		label: "Daylight saving",
		icon: Some("mdi:clock-alert-outline"),
		kind: EntityKind::Switch,
	},
	EntitySpec {
		feature: Feature::Dst,
		family: "dst",
		leaf: "offset",
		label: "Daylight saving offset",
		icon: Some("mdi:clock-fast"),
		kind: EntityKind::Number {
			min: -14.0,
			max: 14.0,
			step: 1.0,
			unit: Some("h"),
			slider: false,
		},
	},
	// ── Redémarrage automatique (101 / 100) ─────────────────────────────
	EntitySpec {
		feature: Feature::AutoReboot,
		family: "autoreboot",
		leaf: "enable",
		label: "Auto reboot",
		icon: Some("mdi:restart-alert"),
		kind: EntityKind::Switch,
	},
	EntitySpec {
		feature: Feature::AutoReboot,
		family: "autoreboot",
		leaf: "weekday",
		label: "Reboot day",
		icon: Some("mdi:calendar-week"),
		kind: EntityKind::Select(&WEEKDAYS),
	},
	EntitySpec {
		feature: Feature::AutoReboot,
		family: "autoreboot",
		leaf: "hour",
		label: "Reboot hour",
		icon: Some("mdi:clock-time-three-outline"),
		kind: EntityKind::Number { min: 0.0, max: 23.0, step: 1.0, unit: Some("h"), slider: false },
	},
	EntitySpec {
		feature: Feature::AutoReboot,
		family: "autoreboot",
		leaf: "minute",
		label: "Reboot minute",
		icon: Some("mdi:clock-time-three-outline"),
		kind: EntityKind::Number { min: 0.0, max: 59.0, step: 1.0, unit: Some("min"), slider: false },
	},
	// ── Système (104 / 105) ─────────────────────────────────────────────
	EntitySpec {
		feature: Feature::General,
		family: "general",
		leaf: "osd_format",
		label: "Date format",
		icon: Some("mdi:calendar-format"),
		kind: EntityKind::Select(&OSD_FORMATS),
	},
	EntitySpec {
		feature: Feature::General,
		family: "general",
		leaf: "time_format",
		label: "Time format (0=24h, 1=12h)",
		icon: Some("mdi:clock-outline"),
		kind: EntityKind::Select(&["0", "1"]),
	},
	EntitySpec {
		feature: Feature::General,
		family: "general",
		leaf: "language",
		label: "Language",
		icon: Some("mdi:web"),
		kind: EntityKind::Text { max_len: 16 },
	},
	EntitySpec {
		feature: Feature::General,
		family: "general",
		leaf: "device_name",
		label: "Device system name",
		icon: Some("mdi:identifier"),
		kind: EntityKind::Text { max_len: 31 },
	},
	EntitySpec {
		feature: Feature::General,
		family: "general",
		leaf: "is_dst",
		label: "Currently in daylight saving",
		icon: Some("mdi:clock-in"),
		kind: EntityKind::FlagSensor { device_class: None },
	},
	// ── Masques de confidentialité (52) — lecture seule ─────────────────
	EntitySpec {
		feature: Feature::PrivacyMask,
		family: "mask",
		leaf: "enable",
		label: "Masking active",
		icon: Some("mdi:eye-off"),
		kind: EntityKind::FlagSensor { device_class: None },
	},
	EntitySpec {
		feature: Feature::PrivacyMask,
		family: "mask",
		leaf: "max_num",
		label: "Mask slots",
		icon: Some("mdi:tag-multiple"),
		// Le nombre d'emplacements est une quantité numérique : elle a droit à
		// `state_class`, que les lignes textuelles, elles, n'auront plus.
		kind: EntityKind::StateSensor {
			unit: None,
			device_class: None,
			class: Some("measurement"),
		},
	},
	// ── Veille (574) et Wi-Fi (115) ─────────────────────────────────────
	EntitySpec {
		feature: Feature::SleepState,
		family: "sleep",
		leaf: "state",
		label: "Sleeping",
		icon: Some("mdi:sleep"),
		kind: EntityKind::FlagSensor { device_class: None },
	},
	EntitySpec {
		feature: Feature::WifiSignal,
		family: "wifi",
		leaf: "signal",
		label: "Wi-Fi signal",
		icon: Some("mdi:wifi"),
		// Aucune unité : le protocole ne dit pas si `<signal>` est en dBm ou
		// en pourcents.
		kind: EntityKind::StateSensor { unit: None, device_class: None, class: Some("measurement") },
	},
	// ── Batterie : les états bon marché de la 253 ───────────────────────
	EntitySpec {
		feature: Feature::BatteryExtra,
		family: "battery",
		leaf: "voltage",
		label: "Battery voltage",
		icon: Some("mdi:flash"),
		kind: EntityKind::StateSensor {
			unit: Some("mV"),
			device_class: Some("voltage"),
			class: Some("measurement"),
		},
	},
	EntitySpec {
		feature: Feature::BatteryExtra,
		family: "battery",
		leaf: "current",
		label: "Battery current",
		icon: Some("mdi:current-dc"),
		kind: EntityKind::StateSensor {
			unit: Some("mA"),
			device_class: Some("current"),
			class: Some("measurement"),
		},
	},
	EntitySpec {
		feature: Feature::BatteryExtra,
		family: "battery",
		leaf: "temperature",
		label: "Battery temperature",
		icon: Some("mdi:thermometer"),
		kind: EntityKind::StateSensor {
			unit: Some("°C"),
			device_class: Some("temperature"),
			class: Some("measurement"),
		},
	},
	EntitySpec {
		feature: Feature::BatteryExtra,
		family: "battery",
		leaf: "charge_status",
		label: "Charge status",
		icon: Some("mdi:battery-charging"),
		kind: EntityKind::StateSensor { unit: None, device_class: None, class: None },
	},
	EntitySpec {
		feature: Feature::BatteryExtra,
		family: "battery",
		leaf: "low_power",
		label: "Battery low",
		icon: Some("mdi:battery-alert"),
		kind: EntityKind::FlagSensor { device_class: Some("battery") },
	},
	// ── Carillon (483-487 / 609 / 610) ─────────────────────────────────
	EntitySpec {
		feature: Feature::Chime,
		family: "chime",
		leaf: "list",
		label: "Paired chimes",
		icon: Some("mdi:bell-outline"),
		// Liste JSON. **Pas d'état du tout quand la caméra a refusé** : « non
		// répondu » doit rester distincte de « liste vide », qui se lit `[]`.
		kind: EntityKind::StateSensor { unit: None, device_class: None, class: None },
	},
	EntitySpec {
		feature: Feature::Chime,
		family: "chime",
		leaf: "volume",
		label: "Chime volume",
		icon: Some("mdi:bell-ring-outline"),
		// `<volLevel>` du carillon, pas un pourcentage : la documentation de
		// référence dit « 0-4 typical » et se garde bien d'affirmer une borne
		// haute (`types.ts` `ChimeParams.volLevel`, `docs/lights.md`). On publie
		// donc la **plage vue**, sans arrondir à 0-100 comme le voudrait le
		// réflexe « volume » : une borne inventée serait une promesse inventée.
		// L'écriture part en 485 `setParam`, qui n'accepte `<volLevel>` que si
		// le `getParam` l'a imprimé — la règle habituelle du « jamais de balise
		// inventée » est appliquée côté écriture, pas ici.
		kind: EntityKind::Number { min: 0.0, max: 4.0, step: 1.0, unit: None, slider: true },
	},
	EntitySpec {
		feature: Feature::Chime,
		family: "chime",
		leaf: "music",
		label: "Chime melody",
		icon: Some("mdi:music-circle-outline"),
		// L'identifiant de mélodie **lu** dans le 485 du carillon interrogé.
		// Verdict seule assumée : aucune commande du protocole n'énumère les
		// mélodies (`id introuvable` dans la note de protocole, §1.4), et une
		// entité `select` dont les options seraient inventées est exactement ce
		// que ce pont refuse de publier. Les identifiants que la caméra imprime
		// elle-même, eux, sont publiés — voir `melodies` juste en dessous.
		kind: EntityKind::StateSensor { unit: None, device_class: None, class: None },
	},
	EntitySpec {
		feature: Feature::Chime,
		family: "chime",
		leaf: "melodies",
		label: "Camera-declared melodies",
		icon: Some("mdi:playlist-music"),
		// Les `musicId` que **la caméra** a imprimés dans ses `<alarminCfg>`
		// (msg 486) et dans son 485, recopiés tels quels. C'est la seule liste
		// de mélodies que ce pont s'autorise à publier : elle vient d'une
		// réponse, pas d'une table. Vide = effacé, jamais « 0 ».
		kind: EntityKind::StateSensor { unit: None, device_class: None, class: None },
	},
	EntitySpec {
		feature: Feature::Chime,
		family: "chime",
		leaf: "ring",
		label: "Ring the chime",
		icon: Some("mdi:bell-ring"),
		// Le payload est un id de mélodie ; le carillon visé est celui appairé
		// à la dernière lecture. Sans carillon appairé la commande échoue, et
		// FAIL le dit.
		kind: EntityKind::Button { payload: "1" },
	},
];

/// Toutes les spécifications d'une famille de discovery, dans l'ordre.
pub fn specs_for(feature: Feature) -> impl Iterator<Item = &'static EntitySpec> {
	SPECS.iter().filter(move |s| s.feature == feature)
}

/// Émet `(topic_config, payload)` pour chaque entité d'une fonctionnalité, dans
/// la forme que sa famille mérite aujourd'hui.
///
/// `ecrivables` = les familles dont la caméra a **répondu à une lecture** et
/// qu'elle accepte de réécrire. Une famille absente de cet ensemble sort en
/// lecture seule (et un bouton n'en sort pas du tout) : c'est la règle
/// « rien d'actionnable avant preuve », appliquée là où elle est vérifiable —
/// la plateforme publiée suit ce qui a dst lu, jamais ce qui est espéré.
pub fn build_feature_entities(
	ctx: &DiscoveryContext,
	feature: Feature,
	writable: &HashSet<Family>,
) -> Vec<(String, Vec<u8>)> {
	let friendly = super::title_case(ctx.camera_name);
	specs_for(feature)
		.filter_map(|spec| {
			// Une famille que le parsing ne connaît pas ne peut pas être
			// relue ni écrite : elle reste lisible, indéfiniment. Le test
			// `every_family_of_a_spec_is_known_of_parsing` vérifie que ce
			// cas ne se présente pas.
			let writable =
				Family::from_slug(spec.family).is_some_and(|f| writable.contains(&f));
			spec.build_from(ctx, &friendly, writable)
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::discovery::{CameraCapabilitiesView, Presence, DiscoveryContext};

	static CAPS: CameraCapabilitiesView = CameraCapabilitiesView {
		has_ptz: false,
		status_led: Presence::Unknown,
		ir_leds: Presence::Unknown,
		ai: Presence::Unknown,
		doorbell: Presence::Unknown,
	};
	static PRESETS: [(u8, String); 0] = [];

	/// Un contexte de discovery minimal, les mêmes valeurs que les tests du
	/// publisher, pour que les topics attendus se lisent d'un coup d'œil.
	fn ctx() -> DiscoveryContext<'static> {
		DiscoveryContext {
			topic_prefix: "bairelay",
			ha_topic: "homeassistant",
			camera_name: "cam1",
			camera_addr: Some("192.0.2.7:9000"),
			camera_uid: Some("UID"),
			sw_version: "1.1.2",
			capabilities: &CAPS,
			presets: &PRESETS,
		}
	}

	#[test]
	fn each_spec_has_a_unique_id() {
		let mut ids: Vec<String> = Vec::new();
		for spec in SPECS {
			let c = ctx();
			let id = spec.unique_id(&c);
			assert!(
				!ids.contains(&id),
				"deux entités partagent le unique_id {id} : HA fusionnerait les deux"
			);
			ids.push(id);
		}
	}

	#[test]
	fn family_and_leaf_are_ascii_slugs() {
		for spec in SPECS {
			for slug in [spec.family, spec.leaf] {
				assert!(
					!slug.is_empty()
						&& slug
							.chars()
							.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
					"slug invalide dans la discovery : {slug:?}"
				);
			}
		}
	}

	#[test]
	fn the_control_topics_match_the_parsing() {
		// Le pont `control/<famille>/<feuille>` n'est accepté que pour les
		// familles/feuilles que `crate::settings` sait parser : une spec qui
		// diverge créerait une entité HA dont la commande est ignorée.
		let c = ctx();
		for spec in SPECS {
			// On teste la forme **modifiable** : c'est elle qui promet un
			// contrôle. La copie lisible, elle, n'a structurellement pas de
			// `command_topic` (le test `une_famille_non_relue...` le vérifie).
			if spec.command_topic(&c, &spec.kind).is_none() {
				continue;
			}
			let (family, leaf) = (spec.family, spec.leaf);
			assert!(
				crate::settings::accepts(family, leaf),
				"discovery émet control/{family}/{leaf} que le parsing refuse"
			);
		}
	}

	#[test]
	fn every_family_of_a_spec_is_known_of_parsing() {
		// Une famille du tableau de discovery que `Family` ne connaît pas ne
		// pourrait jamais être marquée « lue et écrivable » : ses entités
		// resteraient muettes en écriture pour toujours, sans que rien ne le
		// dise. Ce test est la barrière qui empêche cet état.
		for spec in SPECS {
			assert!(
				crate::settings::Family::from_slug(spec.family).is_some(),
				"la spec {}/{} porte une famille que le parsing ne connaît pas",
				spec.family,
				spec.leaf
			);
		}
	}

	#[test]
	fn the_numbers_are_bounded_and_coherent() {
		for spec in SPECS {
			if let EntityKind::Number { min, max, step, .. } = spec.kind {
				assert!(
					min < max && step > 0.0 && step <= (max - min),
					"plage incohérente pour {}/{} : {min}..{max} pas {step}",
					spec.family,
					spec.leaf
				);
			}
		}
	}

	#[test]
	fn payload_number_serialises_the_ha_fields() {
		let c = ctx();
		let spec = SPECS
			.iter()
			.find(|s| s.family == "denoise" && s.leaf == "level")
			.expect("spec denoise/level");
		let (topic, payload) = spec.build(&c, "Porche");
		assert_eq!(
			topic,
			"homeassistant/number/bairelay_cam1_denoise_level/config"
		);
		let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
		assert_eq!(v["min"], 1.0);
		assert_eq!(v["max"], 100.0);
		assert_eq!(v["mode"], "slider");
		assert_eq!(v["unit_of_measurement"], "%");
		assert_eq!(
			v["command_topic"],
			"bairelay/cam1/control/denoise/level",
			"la commande repart vers le topic que le dispatch écoute"
		);
		assert_eq!(
			v["state_topic"],
			"bairelay/cam1/status/denoise/level",
			"l'état vient de la relecture publiée par le dispatch"
		);
		assert_eq!(
			v["availability"],
			serde_json::json!([
				{
					"topic": "bairelay/status",
					"payload_available": "online",
					"payload_not_available": "offline"
				},
				{
					"topic": "bairelay/cam1/presence",
					"payload_available": "reachable",
					"payload_not_available": "unreachable"
				}
			]),
			"deux blocs : le pont, puis la présence de la caméra"
		);
		assert_eq!(v["device"]["identifiers"][0], "bairelay_cam1");
	}

	#[test]
	fn a_read_only_sensor_has_no_command_topic() {
		let c = ctx();
		let mut vus = 0;
		for spec in SPECS {
			// Le même prédicat que le constructeur : si la règle change ici, ce
			// test change avec elle, au lieu de laisser filer un genre nouveau.
			if !is_read_only(&spec.kind) {
				continue;
			}
			vus += 1;
			let (_, payload) = spec.build(&c, "Porche");
			let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
			assert!(
				v.get("command_topic").is_none(),
				"{}/{} est en lecture seule mais expose un command_topic",
				spec.family,
				spec.leaf
			);
		}
		assert!(vus > 0, "aucune spec en lecture seule à vérifier");
	}

	#[test]
	fn the_wifi_sensor_shows_no_unit() {
		let c = ctx();
		let spec = SPECS
			.iter()
			.find(|s| s.family == "wifi")
			.expect("spec wifi");
		let (_, payload) = spec.build(&c, "Porche");
		let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
		assert!(
			v.get("unit_of_measurement").is_none(),
			"le protocole ne définit pas l'unité de <signal> : {v}"
		);
	}

	#[test]
	fn the_chime_button_publishes_a_melody_id() {
		let c = ctx();
		let spec = SPECS
			.iter()
			.find(|s| s.leaf == "ring")
			.expect("spec chime/ring");
		let (topic, payload) = spec.build(&c, "Porche");
		assert_eq!(topic, "homeassistant/button/bairelay_cam1_chime_ring/config");
		let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
		assert_eq!(v["payload_press"], "1");
		assert_eq!(v["command_topic"], "bairelay/cam1/control/chime/ring");
	}

	#[test]
	fn the_select_options_are_the_camera_texts() {
		let c = ctx();
		let spec = SPECS
			.iter()
			.find(|s| s.leaf == "weekday")
			.expect("spec autoreboot/weekday");
		let (_, payload) = spec.build(&c, "Porche");
		let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
		let options: Vec<&str> = v["options"].as_array().expect("options").iter().map(|o| o.as_str().expect("str")).collect();
		assert_eq!(options, WEEKDAYS.to_vec(), "les options sont les textes du protocole, pas des libellés");
	}

	/// Les trois volumes d'`audioCfg` : trois entités, trois identifiants, une
	/// balise XML chacune (documentée sur la ligne du tableau). Le défaut
	/// d'origine n'était pas un libellé maladroit : `<volume>` n'avait aucune
	/// entité, et « Volume interphone » écrivait `<talkAndReplyVolume>` sans le
	/// dire à personne.
	#[test]
	fn the_three_volumes_are_three_separate_entities() {
		let c = ctx();
		let audio: Vec<&EntitySpec> = specs_for(Feature::Audio).collect();
		let attendus = [
			// feuille, libellé, unique_id — l'identifiant du volume général est
			// nouveau, les deux autres sont ceux déjà publiés chez l'opératrice.
			("general_volume", "Speaker volume", "bairelay_cam1_audio_general_volume"),
			("volume", "Intercom volume (talk)", "bairelay_cam1_audio_volume"),
			("visitor_volume", "Visitor volume", "bairelay_cam1_audio_visitor_volume"),
		];
		let mut ids: Vec<&str> = Vec::new();
		for (leaf, label, identifiant) in attendus {
			let spec = audio
				.iter()
				.find(|s| s.leaf == leaf)
				.unwrap_or_else(|| panic!("aucune spec audio/{leaf}"));
			assert_eq!(spec.label, label, "audio/{leaf}");
			assert_eq!(spec.unique_id(&c), identifiant);
			assert!(
				crate::settings::accepts("audio", leaf),
				"la discovery publie audio/{leaf} que le parsing refuse"
			);
			// Et trois commandes distinctes, sinon deux entités écriraient la
			// même balise sous deux noms différents.
			let cmd = crate::settings::SettingCommand::parse(Family::Audio, leaf, "50")
				.unwrap_or_else(|| panic!("audio/{leaf} refusé au parsing"));
			assert_eq!(cmd.family(), Family::Audio);
			assert_eq!(cmd.leaf(), leaf);
			ids.push(spec.leaf);
		}
		let mut tri = ids.clone();
		tri.sort();
		tri.dedup();
		assert_eq!(tri.len(), 3, "{ids:?}");
	}

	/// Tant que la caméra n'a pas répondu à une lecture d'une famille, ses
	/// entités se publient en lecture seule : aucune commande nulle part, et un
	/// bouton n'est pas publié du tout. Relue, la même famille retrouve ses
	/// commandes **sans changer d'identifiant**.
	#[test]
	fn a_family_not_reread_publishes_without_a_command() {
		let c = ctx();
		let non_read: HashSet<Family> = HashSet::new();
		let read: HashSet<Family> = [Family::Audio].into_iter().collect();

		let lectes = build_feature_entities(&c, Feature::Audio, &non_read);
		assert!(!lectes.is_empty(), "la famille audio ne publie plus rien du tout");
		let mut ids_read: Vec<String> = Vec::new();
		for (topic, payload) in &lectes {
			let v: serde_json::Value = serde_json::from_slice(payload).expect("json");
			assert!(
				v.get("command_topic").is_none(),
				"une entité publiée sans lecture porte une commande : {topic}"
			);
			assert!(
				topic.starts_with("homeassistant/sensor/")
					|| topic.starts_with("homeassistant/binary_sensor/"),
				"plateforme modifiable publiée sans lecture : {topic}"
			);
			ids_read.push(v["unique_id"].as_str().expect("unique_id").to_string());
		}

		let writable = build_feature_entities(&c, Feature::Audio, &read);
		let mut ids_modifs: Vec<String> = Vec::new();
		for (topic, payload) in &writable {
			let v: serde_json::Value = serde_json::from_slice(payload).expect("json");
			assert!(v.get("command_topic").is_some(), "{topic} n'offre toujours pas de commande");
			ids_modifs.push(v["unique_id"].as_str().expect("unique_id").to_string());
		}

		// Le bouton `chime/ring` est le seul à disparaître en lecture seule ; la
		// famille audio n'en a pas, donc les deux ensembles se correspondent.
		ids_read.sort();
		ids_modifs.sort();
		assert_eq!(
			ids_read, ids_modifs,
			"les identifiants ne doivent pas dépendre de ce qui a dst lu"
		);
	}

	/// La copie lisible n'est pas un capteur dégénéré : elle garde ce qui rend
	/// l'état compréhensible (l'unité) et perd ce qui serait un mensonge (les
	/// bornes d'écriture, et un `state_class` sur du texte).
	#[test]
	fn the_readable_copy_keeps_the_unit_and_drops_the_bounds() {
		let c = ctx();
		let spec = SPECS
			.iter()
			.find(|s| s.family == "audio" && s.leaf == "general_volume")
			.expect("spec audio/general_volume");
		let (topic, payload) = spec.build_from(&c, "Porche", false).expect("jumeau lisible");
		assert_eq!(
			topic,
			"homeassistant/sensor/bairelay_cam1_audio_general_volume/config",
			"un number lu est un sensor : le topic de configuration change de segment"
		);
		let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
		assert_eq!(v["unit_of_measurement"], "%", "le nombre lu doit rester un pourcentage");
		assert_eq!(v["state_class"], "measurement");
		assert!(v.get("min").is_none() && v.get("max").is_none(), "des bornes sans écriture");
		assert!(v.get("command_topic").is_none());

		// Un interrupteur devient un binary_sensor qui lit les textes que le
		// pont publie réellement (« on »/« off »), pas les « 1 »/« 0 » bruts du
		// protocole — sinon l'entité resterait éteinte quelle que soit la valeur.
		let switch = SPECS
			.iter()
			.find(|s| s.family == "audio" && s.leaf == "visitor_loudspeaker")
			.expect("spec audio/visitor_loudspeaker");
		let (topic, payload) = switch.build_from(&c, "Porche", false).expect("jumeau lisible");
		assert_eq!(
			topic,
			"homeassistant/binary_sensor/bairelay_cam1_audio_visitor_loudspeaker/config"
		);
		let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
		assert_eq!(v["payload_on"], "on");
		assert_eq!(v["payload_off"], "off");

		// Le bouton n'a aucun état à afficher : rien à publier avant lecture.
		let bouton = SPECS
			.iter()
			.find(|s| matches!(s.kind, EntityKind::Button { .. }))
			.expect("une spec button");
		assert!(bouton.build_from(&c, "Porche", false).is_none());
		assert_eq!(bouton.read_only_platform(), None);
	}

	/// Un `text` et un `select` lus deviennent des capteurs **textuels** : un
	/// `state_class: measurement` sur une langue ou un jour de la semaine
	/// remplirait le journal de Home Assistant d'avertissements d'incompatibilité.
	#[test]
	fn a_textual_state_is_never_marked_measured() {
		let c = ctx();
		let mut vus = 0;
		for spec in SPECS {
			if !matches!(spec.kind, EntityKind::Text { .. } | EntityKind::Select(_)) {
				continue;
			}
			vus += 1;
			let (_, payload) = spec.build_from(&c, "Porche", false).expect("jumeau lisible");
			let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
			let ou = format!("{}/{}", spec.family, spec.leaf);
			assert!(v.get("state_class").is_none(), "{ou} mesuré alors que c'est du texte");
			assert!(v.get("unit_of_measurement").is_none(), "{ou} porte une unité");
			assert!(v.get("options").is_none(), "{ou} offre des choix sans commande");
			assert!(
				v.get("command_topic").is_none(),
				"{ou} promet une commande qu'il n'a pas à ce stade"
			);
		}
		assert!(vus >= 2, "aucune spec text/select à tester");
	}

	/// La fabrique de disponibilité est unique : chaque entité de réglage porte
	/// les deux blocs, le pont puis la présence, comme les entités historiques.
	/// Un oubli ici se répéterait sur toutes les entités du tableau.
	#[test]
	fn each_setting_entity_carries_both_availability_blocks() {
		let c = ctx();
		let mut seen = 0;
		for spec in SPECS {
			let (_, payload) = spec.build(&c, "Porche");
			let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
			let blocs = v["availability"].as_array().unwrap_or_else(|| {
				panic!(
					"{}.{} : availability devrait être un tableau",
					spec.family, spec.leaf
				)
			});
			assert_eq!(blocs.len(), 2, "{}.{} : {blocs:?}", spec.family, spec.leaf);
			assert_eq!(
				blocs[0]["topic"],
				"bairelay/status",
				"{}.{} : bloc du pont",
				spec.family, spec.leaf
			);
			assert_eq!(
				blocs[1]["topic"],
				"bairelay/cam1/presence",
				"{}.{} : bloc de présence",
				spec.family, spec.leaf
			);
			assert_eq!(
				blocs[1]["payload_available"],
				"reachable",
				"{}.{}",
				spec.family, spec.leaf
			);
			seen += 1;
		}
		assert!(seen > 0, "aucune spec de réglage n'a dst vérifiée");
	}
}
