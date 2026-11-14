//! Application des réglages MQTT et publication de ce que la caméra a re-lu.
//!
//! Le message `control/<famille>/<feuille>` arrive déjà validé par
//! [`bairelay_mqtt::reglages`] (plage, longueur, texte autorisé) : aucune valeur
//! hors plage n'atteint ici la caméra. Ce module fait trois choses, dans cet
//! ordre :
//!
//! 1. il **relit** le bloc concerné — une écriture Baïchuan est un
//!    lecture-modification-écriture du bloc complet (voir
//!    `docs/protocole-reglages.md`), et ajouter au bloc une balise que le
//!    firmware n'a pas envoyée est la faute que ce pont refuse partout (même
//!    règle que `src/oneshot/reglages_audio.rs::present`) ;
//! 2. il écrit le bloc modifié ;
//! 3. il publie, feuille par feuille, **ce que la caméra a re-lu**.
//!
//! Le point 3 est la loi du module. Un état conservé sur le broker qui répéterait
//! la valeur *demandée* survivrait au redémarrage du pont, masquerait un firmware
//! qui ignore l'écriture, et ferait mentir Home Assistant.
//!
//! Quand la caméra **refuse** une famille, les états conservés de cette famille
//! sont effacés (payload vide conservé : HA affiche « inconnu ») et non remplacés
//! par une valeur par défaut. « Refusé » et « vide » doivent rester distincts —
//! c'est ce qui, sur une sonnette sans carillon appairé, distingue une liste vide
//! légitime (`[]`) d'une caméra qui n'a pas répondu (aucun état).
//!
//! Les réglages ne passent pas par le trait [`CameraDriver`] : ce sont des couples
//! lecture/écriture propres au protocole Baïchuan, et le trait n'a pas vocation à
//! porter trente méthodes. Le dispatch passe donc par le [`BcCamera`] concret
//! ([`crate::camera::CameraHandle::bc_camera_concrete`]).

use bairelay_mqtt::discovery::{EntityKind, SPECS};
use bairelay_mqtt::reglages::{Family, SettingCommand};
use bairelay_mqtt::StatusPublisher;

use bairelay_neolink_core::bc::xml::{
	AiDenoise, AudioCfg, AutoReboot, BatteryInfo, DingDongCfg, Dst, Ntp, RfAlarmCfg, Shelter,
	SleepState, SystemGeneral, WifiSignal,
};
use bairelay_neolink_core::bc_protocol::{BcCamera, Error, ImageCfg, MotionCfg, OsdCfg};

/// Le résultat d'une commande caméra. L'alias du cœur est `pub(crate)` là-bas,
/// donc le binôme `(T, Error)` est écrit ici une fois.
type CoreResult<T> = core::result::Result<T, Error>;

/// Résultat d'une écriture de réglage. `Err` veut dire « la caméra a refusé » (ou
/// a renvoyé un bloc incompréhensible) : le dispatch répondra `FAIL` sur le topic
/// de commande, et **rien n'aura été publié**.
pub(crate) type Result = core::result::Result<(), Error>;

// ── Publication des feuilles ────────────────────────────────────────────
//
// Une feuille = un topic d'état = une entité découverte. Les erreurs de
// publication sont journalisées mais ne remontent pas : ce qui décide de OK/FAIL,
// c'est la caméra, pas le broker.

/// Publie la valeur textuelle d'une feuille, ou l'efface si la caméra n'imprime
/// pas cette balise. Une feuille sans état chez HA veut dire « inconnu » ; y
/// laisser la valeur de la dernière fois voudrait dire « inchangé », ce qui n'est
/// pas ce qu'on sait.
async fn put(publ: &StatusPublisher<'_>, family: Family, leaf: &str, value: Option<&str>) {
	let outcome = match value {
		Some(v) => publ.publish_setting(family, leaf, v).await,
		None => publ.clear_setting(family, leaf).await,
	};
	if let Err(e) = outcome {
		tracing::warn!(
			family = family.slug(),
			leaf,
			error = %e,
			"publication de l'état d'un réglage a échoué"
		);
	}
}

/// Un interrupteur : `on`/`off`, les deux textes que l'entité `switch` découvre.
async fn put_switch(publ: &StatusPublisher<'_>, family: Family, leaf: &str, value: Option<&str>) {
	match on_off_text(value) {
		Some(on) => {
			let text = if on { "on" } else { "off" };
			put(publ, family, leaf, Some(text)).await;
		}
		// `None` couvre deux cas : balise absente, et balise présente avec un
		// texte qu'aucun firmware documenté n'imprime. Dans les deux cas on ne
		// devine pas.
		None => put(publ, family, leaf, None).await,
	}
}

/// Un `binary_sensor` : `1`/`0`.
async fn put_flag(publ: &StatusPublisher<'_>, family: Family, leaf: &str, value: Option<&str>) {
	match on_off_text(value) {
		Some(on) => {
			let text = if on { "1" } else { "0" };
			put(publ, family, leaf, Some(text)).await;
		}
		None => put(publ, family, leaf, None).await,
	}
}

/// Le texte `on`/`off` d'une valeur caméra, ou `None` quand le texte n'est pas
/// l'un des deux états connus. Un `2` lu sur un interrupteur n'est pas « on ».
fn on_off_text(value: Option<&str>) -> Option<bool> {
	match value.map(str::trim)? {
		"1" | "on" | "true" => Some(true),
		"0" | "off" | "false" => Some(false),
		_ => None,
	}
}

/// Le bloc d'une famille en JSON (topic `status/<famille>/json`), pour les
/// automates et le diagnostic. Les entités HA, elles, lisent les feuilles.
async fn publish_json<T: serde::Serialize>(publ: &StatusPublisher<'_>, family: Family, block: &T) {
	match serde_json::to_string(block) {
		Ok(json) => {
			if let Err(e) = publ.publish_settings_json(family, &json).await {
				tracing::warn!(family = family.slug(), error = %e, "publication du JSON d'un réglage a échoué");
			}
		}
		Err(e) => tracing::warn!(family = family.slug(), error = %e, "bloc de réglage non sérialisable en JSON"),
	}
}

/// Compte de feuilles publiées : trois pour l'audio, deux pour le débruitage…
/// Réservé aux journaux et aux tests, qui veulent savoir si une famille a bel et
/// bien parlé.
type Published = core::result::Result<usize, Error>;

async fn publish_audio(publ: &StatusPublisher<'_>, cfg: &AudioCfg) {
	// Les trois volumes du bloc, un par balise XML : `<volume>` (général),
	// `<talkAndReplyVolume>` (parole), `<visitorVolume>` (visiteur).
	// `AudioCfg::volume` est un `String` et non un `Option` : une chaîne vide
	// veut dire « la caméra n'a pas imprimé cette balise », pas « le volume est
	// à zéro ». Y publier `""` serait inventer un état ; on efface.
	let general = Some(cfg.volume.as_str()).filter(|v| !v.trim().is_empty());
	put(publ, Family::Audio, "general_volume", general).await;
	put(publ, Family::Audio, "volume", cfg.talk_and_reply_volume.as_deref()).await;
	put(publ, Family::Audio, "visitor_volume", cfg.visitor_volume.as_deref()).await;
	put_switch(publ, Family::Audio, "visitor_loudspeaker", cfg.visitor_loudspeaker.as_deref()).await;
	publish_json(publ, Family::Audio, cfg).await;
}

async fn publish_denoise(publ: &StatusPublisher<'_>, cfg: &AiDenoise) {
	put_switch(publ, Family::Denoise, "enable", cfg.enable.as_deref()).await;
	put(publ, Family::Denoise, "level", cfg.level.as_deref()).await;
	publish_json(publ, Family::Denoise, cfg).await;
}

async fn publish_image(publ: &StatusPublisher<'_>, cfg: &ImageCfg) {
	let v = &cfg.video_input;
	put(publ, Family::Image, "bright", v.bright.as_deref()).await;
	put(publ, Family::Image, "contrast", v.contrast.as_deref()).await;
	put(publ, Family::Image, "saturation", v.saturation.as_deref()).await;
	put(publ, Family::Image, "hue", v.hue.as_deref()).await;
	put(publ, Family::Image, "sharpen", v.sharpen.as_deref()).await;
	put(publ, Family::Image, "day_night", v.day_night.as_deref()).await;
	publish_json(publ, Family::Image, cfg).await;
}

async fn publish_osd(publ: &StatusPublisher<'_>, cfg: &OsdCfg) {
	put_switch(publ, Family::Osd, "datetime_enable", cfg.datetime.enable.as_deref()).await;
	// Le nom et le filigrane vivent dans le second bloc (44 renvoie les deux).
	let name = cfg.channel_name.as_ref();
	put_switch(publ, Family::Osd, "name_enable", name.and_then(|c| c.enable.as_deref())).await;
	put(publ, Family::Osd, "name", name.and_then(|c| c.name.as_deref())).await;
	put_switch(publ, Family::Osd, "watermark", name.and_then(|c| c.en_watermark.as_deref())).await;
	publish_json(publ, Family::Osd, cfg).await;
}

async fn publish_motion(publ: &StatusPublisher<'_>, cfg: &MotionCfg) {
	put_switch(publ, Family::Motion, "enable", cfg.enable()).await;
	put(publ, Family::Motion, "sensitivity", cfg.sensitivity()).await;
	// Pas de JSON ici : la forme `<MdAlarm>` (seule réponse de certaines
	// caméras au msg 46) a des champs non énumérés dans le protocole, et les
	// publier n'apporte rien que les deux feuilles ci-dessus ne disent.
}

/// Le PIR a deux topics d'état : l'interrupteur historique
/// (`status/pir`, posé par le dispatch) et sa sensibilité. `pub(crate)` parce
/// que l'écriture de l'interrupteur (`control/pir`) passe par le dispatch
/// historique et veut la même règle d'orthographe.
pub(crate) async fn publish_pir(publ: &StatusPublisher<'_>, cfg: &RfAlarmCfg) {
	// `sensiValue` (firmware v1.1+) et `sensitivity` (ancien) sont deux
	// orthographes de la même molette ; on publie celle qui existe.
	let sens = cfg
		.sensiValue
		.map(|v| v.to_string())
		.or_else(|| cfg.sensitivity.map(|v| v.to_string()));
	put(publ, Family::Pir, "sensitivity", sens.as_deref()).await;
	// Le temps de repos entre deux alarmes, en secondes, quand le firmware
	// l'imprime. `intervalUseRange` n'est pas publié en entité séparée : il est
	// dans le JSON ci-dessous, et c'est là qu'il faut aller le lire si la valeur
	// écrite ne survit pas à la relecture.
	put(
		publ,
		Family::Pir,
		"interval",
		cfg.interval.map(|v| v.to_string()).as_deref(),
	)
	.await;
	publish_json(publ, Family::Pir, cfg).await;
}

async fn publish_ntp(publ: &StatusPublisher<'_>, cfg: &Ntp) {
	put_switch(publ, Family::Ntp, "enable", cfg.enable.as_deref()).await;
	put(publ, Family::Ntp, "server", cfg.server.as_deref()).await;
	put(publ, Family::Ntp, "interval", cfg.synchronize_interval.as_deref()).await;
	publish_json(publ, Family::Ntp, cfg).await;
}

async fn publish_dst(publ: &StatusPublisher<'_>, cfg: &Dst) {
	let enable = cfg.enable.map(|v| v.to_string());
	let offset = cfg.offset.map(|v| v.to_string());
	put_switch(publ, Family::Dst, "enable", enable.as_deref()).await;
	put(publ, Family::Dst, "offset", offset.as_deref()).await;
	publish_json(publ, Family::Dst, cfg).await;
}

async fn publish_auto_reboot(publ: &StatusPublisher<'_>, cfg: &AutoReboot) {
	put_switch(publ, Family::AutoReboot, "enable", cfg.enable.as_deref()).await;
	put(publ, Family::AutoReboot, "weekday", cfg.week_day.as_deref()).await;
	put(publ, Family::AutoReboot, "hour", cfg.hour.as_deref()).await;
	put(publ, Family::AutoReboot, "minute", cfg.minute.as_deref()).await;
	publish_json(publ, Family::AutoReboot, cfg).await;
}

async fn publish_general(publ: &StatusPublisher<'_>, cfg: &SystemGeneral) {
	put(publ, Family::General, "osd_format", cfg.osd_format.as_deref()).await;
	let time_format = cfg.time_format.map(|v| v.to_string());
	put(publ, Family::General, "time_format", time_format.as_deref()).await;
	put(publ, Family::General, "language", cfg.language.as_deref()).await;
	put(publ, Family::General, "device_name", cfg.device_name.as_deref()).await;
	// `isDst` est en lecture seule au protocole : la caméra le calcule. Un
	// capteur, donc, pas un interrupteur.
	let is_dst = cfg.is_dst.map(|v| v.to_string());
	put_flag(publ, Family::General, "is_dst", is_dst.as_deref()).await;
	publish_json(publ, Family::General, cfg).await;
}

async fn publish_masks(publ: &StatusPublisher<'_>, cfg: &Shelter) {
	put_flag(publ, Family::Mask, "enable", cfg.enable.as_deref()).await;
	put(publ, Family::Mask, "max_num", cfg.max_num.as_deref()).await;
	publish_json(publ, Family::Mask, cfg).await;
}

async fn publish_sleep(publ: &StatusPublisher<'_>, cfg: &SleepState) {
	put_flag(publ, Family::Sleep, "state", cfg.sleep.as_deref()).await;
}

async fn publish_wifi(publ: &StatusPublisher<'_>, cfg: &WifiSignal) {
	put(publ, Family::Wifi, "signal", cfg.signal.as_deref()).await;
}

/// Les états bonus de la trame 253. Pas de JSON pour cette famille :
/// `<BatteryInfo>` porte un `<username>` et les jetons `system`/`network`/`alarm`
/// de la caméra, qui n'ont rien à faire dans un état conservé sur un broker.
async fn publish_battery(publ: &StatusPublisher<'_>, cfg: &BatteryInfo) {
	put(publ, Family::Battery, "voltage", Some(&cfg.voltage.to_string())).await;
	put(publ, Family::Battery, "current", Some(&cfg.current.to_string())).await;
	put(publ, Family::Battery, "temperature", Some(&cfg.temperature.to_string())).await;
	put(publ, Family::Battery, "charge_status", Some(cfg.charge_status.as_str())).await;
	put(publ, Family::Battery, "low_power", Some(&cfg.low_power.to_string())).await;
}

/// La liste des carillons appairés, en JSON.
///
/// `chimes == None` (le msg 484 a refusé) ne publie **rien** : c'est « la caméra
/// n'a pas répondu », et non « il n'y a pas de carillon ». `Some(vec![])` publie
/// `[]`, qui lui, veut bien dire « appairé : aucun ».
async fn publish_chime(publ: &StatusPublisher<'_>, cfg: &DingDongCfg) {
	match cfg.chimes.as_ref() {
		Some(list) => {
			let items: Vec<serde_json::Value> = list
				.iter()
				// Un bloc sans id adressable n'est pas un carillon : la règle du
				// 484 (note de protocole §1.2) est de l'ignorer, et le publier
				// `{"id": null}` ferait croire à une entrée exploitable.
				.filter(|d| d.identifier().is_some())
				.map(|d| {
					serde_json::json!({
						"id": d.identifier(),
						"name": d.device_name.as_deref().or(d.name.as_deref()),
						// `online` reste strict : `true` seulement sur `<netState>1</netState>`,
						// la valeur documentée. La caméra de terrain écrit **2** pour un
						// carillon appairé qui répond à ses lectures par carillon ; redéfinir
						// 2 en ligne serait inventer. Le texte brut est donc publié à côté,
						// et c'est lui que l'opératrice lit.
						"online": d.is_online(),
						"netstate": d.net_state(),
					})
				})
				.collect();
			let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
			put(publ, Family::Chime, "list", Some(&json)).await;
		}
		None => put(publ, Family::Chime, "list", None).await,
	}
	// Volume et mélodie viennent du 485 (`dingdongDeviceOpt`), qui n'est interrogé
	// que si un carillon a été trouvé. Absent = aucun état, pas un « 0 ».
	let params = cfg.params.as_ref();
	put(publ, Family::Chime, "volume", params.and_then(|p| p.vol_level.as_deref())).await;
	put(publ, Family::Chime, "music", params.and_then(|p| p.music_id.as_deref())).await;
	// Les mélodies que **la caméra** a imprimées : les `musicId` de ses
	// `<alarminCfg>` (486), plus celui de son 485. C'est la seule liste de
	// mélodies que ce pont publie — aucune table locale, aucun id inventé (§1.4 :
	// le listing des mélodies est `id introuvable`). Vide = rien, pas « 0 ».
	let param_melodie =
		params.and_then(|p| p.music_id.as_deref()).map(str::trim).filter(|m| !m.is_empty());
	let evenements = cfg.events.as_deref().unwrap_or(&[]);
	let mut melodies: Vec<&str> = evenements
		.iter()
		.flat_map(|bloc| bloc.alarmin_cfg.iter())
		.filter_map(|alarme| alarme.music_id.as_deref())
		.map(str::trim)
		.filter(|m: &&str| !m.is_empty())
		.chain(param_melodie)
		.collect();
	melodies.sort_by_key(|m: &&str| m.parse::<u32>().ok().map_or(u32::MAX, |n| n));
	melodies.dedup();
	let melodies = if melodies.is_empty() { None } else { Some(melodies.join(",")) };
	put(publ, Family::Chime, "melodies", melodies.as_deref()).await;
	// Le bloc complet en JSON (`status/chime/json`) : c'est lui qui permet de
	// vérifier sur le terrain ce que la caméra a réellement répondu sur
	// 483/484/485/486/609, sans brancher un renifleur sur la caméra.
	publish_json(publ, Family::Chime, cfg).await;
}

// ── Lecture seule : synchronisation à la demande ────────────────────────

/// Les feuilles que la découverte Home Assistant déclare pour une famille, hors
/// boutons (un bouton n'a pas d'état à effacer).
fn leaves_of(family: Family) -> impl Iterator<Item = &'static str> {
	SPECS
		.iter()
		.filter(move |s| s.family == family.slug())
		.filter(|s| !matches!(s.kind, EntityKind::Button { .. }))
		.map(|s| s.leaf)
}

/// Efface tous les états d'une famille : ce qu'on fait quand elle a refusé.
///
/// Le motif de ce refus se publie à côté, pas à la place : voir
/// [`publier_motif`]. Effacer sans expliquer redonne exactement le silence que
/// l'opératrice ne peut pas diagnostiquer.
pub(crate) async fn clear_family(publ: &StatusPublisher<'_>, family: Family) {
	for leaf in leaves_of(family) {
		put(publ, family, leaf, None).await;
	}
	let _ = publ.clear_settings_json(family).await;
}

/// Le motif qu'une famille n'a pas répondu dans le délai. Le timeout est déclenché
/// par le dispatch, pas par le cœur protocole : c'est ici qu'on sait le dire.
pub(crate) const MOTIF_TIMEOUT: &str = "la caméra n'a pas répondu à temps";

/// Met une famille en état d'être lue sans être écrite, et le dit.
pub(crate) const MOTIF_LECTURE_SEULE: &str =
	"la caméra lit ce réglage mais refuse qu'on lui renvoie le bloc : lecture seule";

/// Le pont n'a pas de session Baïchuan en cours : la commande n'a pas pu partir.
/// Ce n'est pas un refus de la caméra, mais c'est un refus — et un refus qui se
/// tait est indistinguable d'un pont mort.
pub(crate) const MOTIF_SANS_SESSION: &str =
	"aucune session Baïchuan en cours : rien n'a été envoyé à la caméra";

/// Ce que la caméra vient de répondre était mauvais **une fois** : le réglage
/// n'est pas verrouillé pour autant, et le topic doit le dire. Sans ce suffixe,
/// un `400` lu pendant une salve se lirait comme une interdiction définitive —
/// exactement la fausse promesse inverse de celle que ce module corrige.
///
/// Formé en deux parties parce que la première moitié est le motif technique,
/// variable selon l'erreur : voir [`publier_motif_indetermine`].
pub(crate) const MOTIF_SANS_VERROUILLAGE: &str =
	" — réglage non verrouillé : la famille garde son état";

/// La conséquence d'un refus **transitoire**, publiée cette fois **en tête** de
/// la phrase : [`raison_publiable`] coupe à 160 caractères en gardant le début,
/// donc une fin de phrase ne survit pas à un motif technique long (`la réponse
/// de la caméra est incompréhensible : …`). Or c'est précisément la moitié que
/// l'opératrice regarde : la famille est-elle utilisable ou non.
pub(crate) const MOTIF_TRANSITOIRE: &str =
	"erreur de lecture transitoire : la famille reste publiée modifiable";

/// Refus **local** et structurel : la balise qu'il faudrait renvoyer pour écrire
/// n'est pas dans la réponse de la caméra. Ce n'est pas la caméra qui a répondu
/// mauvais, c'est nous qui ne pouvons pas écrire sans inventer une balise — la
/// seule chose que ce pont refuse partout. Nommer la balise est le but de la
/// phrase : c'est ce qui reste à lire quand on n'a pas la caméra sous la main.
pub(crate) const MOTIF_SANS_OSDCHANNEL: &str = concat!(
	"la réponse de la caméra ne contient pas <OsdChannelName> : ",
	"rien à lui renvoyer, lecture seule"
);

/// Refus **local**, lui aussi : la caméra a bien répondu au 484 (liste des
/// carillons appairés), mais aucune entrée de cette réponse n'a porté un
/// identifiant exploitable — sous l'une comme sous l'autre orthographe
/// documentées du porteur (`<dingdongDeviceInfo>`, `<dingdongList>`). C'est
/// exactement la situation de terrain observée : un carillon physique appairé
/// et une liste publiée vide. Le motif le dit au lieu de publier `[]` en
/// silence, parce que `[]` se lit « aucun carillon » alors qu'il faut lire « la
/// caméra n'énumère pas celui-ci ».
pub(crate) const MOTIF_CARILLON_SANS_ENTREE: &str = concat!(
	"484 a répondu sans aucune entrée reconnue : carillon non énuméré, ",
	"donc rien à adresser en 485"
);

/// Même verdict, autre cause : le 484 n'a pas répondu du tout (modèle sans
/// carillon sans fil, ou refus). La nuance compte, elle est donc distincte.
pub(crate) const MOTIF_CARILLON_SANS_ID: &str = concat!(
	"la caméra n'a pas répondu à 484 : aucun identifiant de carillon, ",
	"donc rien à adresser en 485"
);

/// Les entités de reconnaissance publient un état et **aucun** topic de
/// commande, et ce n'est pas un oubli du pont : la balise qui allume un type de
/// reconnaissance n'a jamais été lue. `342`/`343` (`<AiDetectCfg>`, interrogé
/// **par type**) ne documentent que `<sensitivity>`, `<stayTime>` et les tailles
/// de cible. Publier un `command_topic` ici serait promettre l'écriture d'une
/// balise inventée ; le motif nomme donc ce qu'il faut rapporter du terrain.
pub(crate) const MOTIF_AI_SANS_BALISE: &str = concat!(
	"aucune balise d'activation connue par type (342/AiDetectCfg ne documente que ",
	"sensibilite, stayTime, tailles) : pas de topic de commande"
);

/// Publie le motif d'un refus sur `status/<famille>/reason` (conservé).
///
/// La phrase est courte, en français quand le pont sait la former, et nettoyée
/// par [`bairelay_mqtt::status::raison_publiable`] : ni XML brut, ni identifiant,
/// ni secret. Une erreur de publication ne remonte pas — le broker n'est pas ce
/// qui décide de OK/FAIL — mais elle est journalisée.
pub(crate) async fn publier_motif(publ: &StatusPublisher<'_>, family: Family, raison: &str) {
	if let Err(e) = publ.publish_family_reason(family, raison).await {
		tracing::warn!(family = family.slug(), error = %e, "publication du motif d'un refus a échoué");
	}
}

/// Publie le motif d'un refus **non structurel** : la caméra a répondu mauvais,
/// ou pas répondu, et le pont ne change rien d'autre que ce topic.
///
/// La phrase porte le motif technique **et** la conséquence pour Home Assistant
/// ([`MOTIF_SANS_VERROUILLAGE`]) : ce qui est écrit ici ne verrouille aucune
/// entité. C'est la moitié que l'opératrice regarde en premier, et sans elle un
/// `400` pendant une salve de quinze commandes ressemble à un réglage interdit.
pub(crate) async fn publier_motif_indetermine(
	publ: &StatusPublisher<'_>,
	family: Family,
	raison: &str,
) {
	publier_motif(publ, family, &format!("{raison}{MOTIF_SANS_VERROUILLAGE}")).await;
}

/// Publie le motif d'un refus **transitoire** : la caméra a répondu mauvais, trop
/// tard, ou en un bloc inexploitable, et la famille **reste publiée modifiable**.
///
/// La conséquence ([`MOTIF_TRANSITOIRE`]) passe en tête, avant le motif technique
/// : c'est la moitié que l'opératrice regarde et elle doit survivre à la coupe à
/// 160 caractères. Un `400` arraché pendant une salve de quinze commandes radio ne
/// doit plus jamais ressembler à un réglage interdit.
pub(crate) async fn publier_motif_transitoire(
	publ: &StatusPublisher<'_>,
	family: Family,
	raison: &str,
) {
	publier_motif(publ, family, &format!("{MOTIF_TRANSITOIRE}, {raison}")).await;
}

/// Fait disparaître le motif : la famille vient de répondre, la raison d'un
/// refus passé n'a plus rien à faire sur le broker.
pub(crate) async fn effacer_motif(publ: &StatusPublisher<'_>, family: Family) {
	if let Err(e) = publ.clear_family_reason(family).await {
		tracing::warn!(family = family.slug(), error = %e, "effacement du motif d'un refus a échoué");
	}
}

/// Traduit une erreur du cœur protocole en la phrase courte que l'opératrice
/// lira sur le topic `reason`.
///
/// Les variantes structurées ont une phrase française à elles — ce sont les
/// refus qu'on rencontre vraiment, et ils méritent d'être compris sans ouvrir
/// la doc du protocole. Les autres cas gardent le message d'origine après une
/// introduction française : c'est souvent le seul détail qui reste, et
/// `raison_publiable` garantit qu'il ne débordera pas en XML ou en secret.
pub(crate) fn motif(err: &Error) -> String {
	match err {
		Error::Timeout(_)
		| Error::TimeoutDisconnected
		| Error::TimeoutError(_)
		| Error::DiscoveryTimeout => MOTIF_TIMEOUT.to_string(),
		Error::CameraServiceUnavailable { code, .. } => {
			format!("la caméra a refusé la commande (code {code})")
		}
		// Le même refus, quand c'est une **lecture de réglage** qui l'a rencontré :
		// la caméra a écrit une réponse avec son code, et ce texte est la seule
		// chose qui dise pourquoi. Il arrive déjà borné du noyau et
		// `raison_publiable` le met en une ligne sans balises avant qu'il touche
		// le broker — les noms de balises restent lisibles, ce sont souvent
		// l'information.
		Error::CameraRefused { code, detail, .. } => {
			format!("la caméra a refusé la commande (code {code}) — elle a répondu : {detail}")
		}
		Error::CameraLoginFail => "la caméra a refusé l'identification".to_string(),
		Error::ConnectionUnavailable | Error::CannotInitCamera => {
			"la caméra est injoignable".to_string()
		}
		Error::DroppedConnection
		| Error::DroppedConnectionTry(_)
		| Error::ConnectionShutdown
		| Error::TokioBcSendError => "la connexion à la caméra s'est interrompue".to_string(),
		Error::MissingAbility { name, .. } => {
			format!("la caméra ne déclare pas la capacité « {name} » pour ce réglage")
		}
		Error::UnintelligibleXml { why, .. } | Error::UnintelligibleReply { why, .. } => {
			format!("la réponse de la caméra est incompréhensible : {why}")
		}
		// `Usage` et `Other` portent déjà une phrase, mais préfixée en anglais
		// par leur `Display` (« Usage error: … »). On reprend le texte seul pour
		// que la ligne lue chez Home Assistant commence par du français. Les deux
		// bras sont séparés parce que `Usage` porte un `String` et `Other` un
		// `&'static str` : un motif commun ne peut pas lier les deux.
		Error::Usage(why) => format!("réglage refusé : {why}"),
		Error::Other(why) => format!("réglage refusé : {why}"),
		other => format!("réglage refusé : {other}"),
	}
}

/// Ce que la découverte doit retenir d'une tentative de lecture d'une famille.
///
/// Quatre verdicts, pas deux, parce que quatre situations n'enseignent pas la
/// même chose. Les confondre est la régression relevée sur le terrain : une
/// famille écrite avec succès, puis refusée d'un `400` au milieu d'une salve de
/// quinze commandes, retombait en lecture seule parce que le pont lisait dans un
/// échec de lecture une preuve que l'écriture est impossible. Elle ne l'est pas
/// — et le terrain l'a montré deux fois : `image` (26) répond `400` à la
/// **lecture** et applique pourtant l'écriture du contraste, relue et vérifiée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lecture {
	/// La caméra a donné son bloc **et** nous savons le lui renvoyer : la
	/// famille est modifiable. Une écriture qui aboutit est la même preuve, en
	/// plus fort.
	Ecrivable,
	/// Preuve **locale** et structurelle que l'écriture est impossible : le
	/// porteur lu ne se renvoie pas (`<MdAlarm>`, refusé par
	/// [`BcCamera::set_motion_cfg`]), ou la balise à modifier n'est pas dans la
	/// réponse (`<OsdChannelName>` pour le nom et le filigrane). Dans les deux
	/// cas c'est **nous** qui refusons, pas la caméra : ce sont les seuls
	/// verdicts, avec celui de la famille sans bloc lu du tout, qui verrouillent.
	NonEcrivable,
	/// La caméra a répondu mauvais, trop tard, ou en un bloc incompréhensible :
	/// `400`, délai dépassé, réponse inexploitable. Ça ne prouve **rien** contre
	/// l'écriture — un firmware endormi répond `400` aux blocs vidéo et les
	/// redonne vingt secondes plus tard, et `image` est l'exemple vécu où la
	/// lecture refuse alors que l'écriture passe. La famille est donc publiée
	/// **modifiable**, comme avant ce verdict, et la raison du refus est publiée
	/// à côté : c'est la paire que l'opératrice demandait, « utilisable mais pas
	/// muet ».
	Transitoire,
	/// Rien n'a pu être tenté du tout : pas de session Baïchuan, identification
	/// refusée, caméra injoignable, capacité absente. Ce n'est pas une réponse de
	/// la caméra, donc le pont ne promet rien qu'il n'ait pu vérifier : la
	/// découverte ne change **rien** ; le motif, lui, se publie.
	Indetermine,
}

impl Lecture {
	/// Le booléen que comprend le publieur de découverte. `None` = « ne change
	/// rien à ce qui est publié » ([`Lecture::Indetermine`]).
	pub(crate) fn ecrivable(self) -> Option<bool> {
		match self {
			Lecture::Ecrivable => Some(true),
			Lecture::NonEcrivable => Some(false),
			// Une lecture qui n'a pas abouti pour une raison passagère ne retire
			// pas une commande qui fonctionnait : c'est le cœur de la correction.
			Lecture::Transitoire => Some(true),
			Lecture::Indetermine => None,
		}
	}
}

/// Ce que la branche de [`sync_family`] a compris du porteur qu'elle vient de
/// lire.
///
/// Ces drapeaux voyagent par paramètre plutôt que par la valeur de retour : les
/// quinze branches de [`sync_family`] sont des blocs `async` séparés pour des
/// raisons de pile (voir leur doc) et rendent chacune son nombre de feuilles.
/// Changer quinze signatures pour une information que trois branches seulement
/// connaît serait le pire endroit pour la payer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Portee {
	/// La caméra accepte qu'on lui renvoie le bloc qu'elle vient d'envoyer.
	pub ecrivable: bool,
	/// La phrase qui dit pourquoi elle ne l'accepte pas, quand le cas général
	/// ([`MOTIF_LECTURE_SEULE`]) ne suffit pas à nommer la balise en cause.
	pub motif: Option<&'static str>,
}

impl Portee {
	/// Verdict neuf : « réécrivable, sans rien à signaler ».
	pub(crate) fn nouvelle() -> Self {
		Portee { ecrivable: true, motif: None }
	}
}

/// Lit une famille, publie ses feuilles, et met le topic `reason` à jour : vide
/// quand la famille est en bonne santé, la phrase du refus sinon. Répond
/// `(famille, verdict)`, c'est-à-dire ce que la découverte doit publier — ou ne
/// pas publier du tout ([`Lecture::Indetermine`]).
///
/// C'est **là**, et pas chez l'appelant, que la famille bascule : lecture
/// réussie → plus aucun motif ; refus → états effacés **et** motif publié. Les
/// deux moitiés de la règle (« plus jamais silencieux » / « refusé ≠ vide »)
/// restent ainsi dans le même bloc.
///
/// Le refus, justement, a trois espèces, et une seule verrouille :
///
/// * **la preuve locale** — porteur qui ne se renvoie pas, balise absente : elle
///   est prise dans [`sync_family`] elle-même, en `Ok`, et donne
///   [`Lecture::NonEcrivable`] ;
/// * **la réponse maussade** — `400`, délai dépassé, bloc incompréhensible : elle
///   ne prouve rien du tout et donne [`Lecture::Transitoire`]. La famille se
///   publie modifiable, comme avant, avec la raison du refus écrit à côté ;
/// * **la tentative impossible** — pas de session, caméra injoignable : le pont
///   n'a rien pu vérifier et rend [`Lecture::Indetermine`], qui ne change rien à
///   la découverte.
///
/// Sur un refus qui ressemble à un « pas maintenant », une **seconde** tentative
/// est faite ici même : les GET des réglages n'ont pas la boucle de rejeu que le
/// dépôt applique déjà ailleurs au même code, et sans elle une salve complète
/// perd les quatre blocs que la caméra finit par donner quand elle s'est
/// réveillée. Une seule tentative de plus, pas davantage : `query/reglages` est
/// déjà une quinzaine de commandes radio sur une sonnette sur batterie. Le délai
/// dépassé, lui, n'est pas rejoué — re-tenter une commande à laquelle la caméra
/// n'a pas répondu, c'est payer deux fois une radio qui dort.
pub(crate) async fn lire_famille(
	bc: &BcCamera,
	publ: &StatusPublisher<'_>,
	family: Family,
) -> (Family, Lecture) {
	let mut portee = Portee::nouvelle();
	match sync_family(bc, publ, family, &mut portee).await {
		Ok(n) => promettre(publ, family, portee, n).await,
		Err(e) if est_reessayable(&e) => {
			tracing::debug!(
				family = family.slug(),
				error = %e,
				"lecture de réglage refusée d'un code transitoire : une seconde tentative"
			);
			let mut seconde = Portee::nouvelle();
			match sync_family(bc, publ, family, &mut seconde).await {
				Ok(n) => promettre(publ, family, seconde, n).await,
				Err(deuxieme) => repli(publ, family, &deuxieme).await,
			}
		}
		Err(e) => repli(publ, family, &e).await,
	}
}

/// Batterie **déjà lue** par l'appelant : publie les cinq champs et rend le même
/// verdict que [`lire_famille`], sans repayer la trame 253.
///
/// Il existe parce que la salve de réveil ([`crate::salve`]) lit la batterie
/// elle-même — c'est pour son niveau qu'elle se déclenche — et doit quand même
/// publier les motifs et le verdict comme le fait `query/reglages`. Deux chemins
/// qui publient la batterie avec deux règles de verdict, c'est exactement la
/// divergence que ce module refuse partout ailleurs.
pub(crate) async fn batterie_lue(
	publ: &StatusPublisher<'_>,
	issu: &CoreResult<BatteryInfo>,
) -> (Family, Lecture) {
	match issu {
		Ok(_) => promettre(publ, Family::Battery, Portee::nouvelle(), 5).await,
		Err(e) => repli(publ, Family::Battery, e).await,
	}
}

/// La famille a répondu son bloc. Publie le verdict qui en découle : motif vidé
/// si nous savons le lui renvoyer, sinon la phrase qui explique pourquoi l'écriture
/// ne sera jamais promise. Les états, eux, restent en place — ils sont justes.
async fn promettre(
	publ: &StatusPublisher<'_>,
	family: Family,
	portee: Portee,
	feuilles: usize,
) -> (Family, Lecture) {
	tracing::debug!(
		family = family.slug(),
		leaves = feuilles,
		ecrivable = portee.ecrivable,
		"réglages synchronisés"
	);
	if portee.ecrivable {
		effacer_motif(publ, family).await;
		return (family, Lecture::Ecrivable);
	}
	// Pas un refus de la caméra : une promesse que nous ne ferons pas.
	publier_motif(publ, family, portee.motif.unwrap_or(MOTIF_LECTURE_SEULE)).await;
	(family, Lecture::NonEcrivable)
}

/// La caméra n'a rien donné d'exploitable : c'est le repli, et c'est lui qui
/// décide quelle espèce de manque nous avons sous les yeux.
///
/// Dans les deux cas les états conservés sont effacés — « on ne sait pas » ne se
/// rend pas avec la valeur de la dernière fois — et le motif est publié. Ce qui
/// change est le verdict :
///
/// * **passable** (400, délai, bloc incompréhensible) → [`Lecture::Transitoire`] :
///   la famille se publie modifiable avec la raison à côté, parce qu'un refus de
///   **lecture** n'a jamais rien prouvé contre l'**écriture** ;
/// * **tentative impossible** (pas de session, caméra injoignable, capacité
///   absente) → [`Lecture::Indetermine`] : le pont ne peut promettre ce qu'il n'a
///   pas pu tenter, la découverte garde ce qu'elle avait.
async fn repli(publ: &StatusPublisher<'_>, family: Family, e: &Error) -> (Family, Lecture) {
	clear_family(publ, family).await;
	if est_transitoire(e) {
		tracing::debug!(
			family = family.slug(),
			error = %e,
			"lecture de réglages passagèrement refusée : états effacés, motif publié, famille laissée modifiable"
		);
		publier_motif_transitoire(publ, family, &motif(e)).await;
		return (family, Lecture::Transitoire);
	}
	tracing::debug!(
		family = family.slug(),
		error = %e,
		"lecture de réglages non concluante : états effacés, motif publié, verdict inchangé"
	);
	publier_motif_indetermine(publ, family, &motif(e)).await;
	(family, Lecture::Indetermine)
}

/// Ce refus est-il une **raison passagère**, qui ne dit rien de l'écriture ?
///
/// Trois familles de symptômes, et elles sont les trois que le terrain a vues :
/// le `400` de ce dialecte, qui veut dire « pas maintenant » et non « pas ça » ;
/// le délai dépassé, quand la sonnette dort ou que quinze commandes d'affilée
/// l'ont essoufflée ; et le bloc reçu mais inutilisable, qu'un firmware endormi
/// tronque ou renvoie vide. Un `500`, un refus d'identification ou une connexion
/// tombée ne sont **pas** de ce lot : là, le pont n'a rien pu vérifier du tout.
fn est_transitoire(e: &Error) -> bool {
	match e {
		Error::CameraServiceUnavailable { code: 400, .. } => true,
		// Un refus de lecture avec texte reste un `400` : même verdict, et le
		// texte ne change rien à la nature passagère du refus.
		Error::CameraRefused { code: 400, .. } => true,
		Error::Timeout(_)
		| Error::TimeoutDisconnected
		| Error::TimeoutError(_)
		| Error::DiscoveryTimeout
		| Error::UnintelligibleXml { .. }
		| Error::UnintelligibleReply { .. } => true,
		_ => false,
	}
}

/// Ce refus mérite-t-il la **seconde** tentative de [`lire_famille`] ? Seul le
/// `400` : c'est le seul que le dépôt traite déjà comme réessayable — voir la
/// boucle de `bc_protocol::services.rs`, qui en tente cinq avant de se rendre.
/// Un délai dépassé se paie deux fois en radio sur une caméra sur batterie, et
/// une réponse incompréhensible ne s'améliore pas en la redemandant.
fn est_reessayable(e: &Error) -> bool {
	matches!(
		e,
		Error::CameraServiceUnavailable { code: 400, .. } | Error::CameraRefused { code: 400, .. }
	)
}

/// Lit une famille et publie ses feuilles. Le `Ok` porte le nombre de feuilles
/// publiées, pour les journaux et les tests.
///
/// En cas d'échec (refus, timeout, bloc incompréhensible), l'erreur remonte et
/// le dispatch efface les états conservés de la famille. Cet appel coûte au moins
/// une commande caméra : il est réservé à la demande explicite
/// (`query/reglages`), jamais à un cycle tournant.
///
/// Chaque branche est un bloc `async` qui lui appartient, et ce n'est pas de
/// l'esthétique : les blocs du protocole (`ImageCfg`, `SystemGeneral`,
/// `DingDongCfg`…) font plusieurs kilo-octets, et un cadre de pile non optimisé
/// garde tous leurs temporaires vivants à la fois. Une seule fonction
/// manipulant les quinze blocs ensemble dépassait la pile du thread qui
/// dispatche — 2 Mo pour un worker tokio, et c'est exactement ce chemin-là qui
/// doit tenir dedans. Un bloc par famille : chaque cadre ne connaît plus que le
/// bloc qu'il est en train de relire.
pub(crate) async fn sync_family(
	bc: &BcCamera,
	publ: &StatusPublisher<'_>,
	family: Family,
	portee: &mut Portee,
) -> Published {
	match family {
		Family::Audio => async {
			let cfg = bc.get_audio_cfg(false).await?;
			publish_audio(publ, &cfg).await;
			// Quatre feuilles depuis que le troisième volume a son entité :
			// `<volume>`, `<talkAndReplyVolume>`, `<visitorVolume>`,
			// `<visitorLoudspeaker>`.
			Ok(4)
		}
		.await,
		Family::Denoise => async {
			let cfg = bc.get_ai_denoise().await?;
			publish_denoise(publ, &cfg).await;
			Ok(2)
		}
		.await,
		Family::Image => async {
			let cfg = bc.get_image_cfg().await?;
			publish_image(publ, &cfg).await;
			Ok(6)
		}
		.await,
		Family::Osd => async {
			let cfg = bc.get_osd_cfg().await?;
			// Trois des quatre feuilles de l'OSD — le nom affiché, l'affichage
			// du nom, le filigrane — vivent dans `<OsdChannelName>`. Sans
			// cette balise nous n'avons rien à renvoyer : c'est notre refus, pas
			// celui de la caméra, et c'est le seul qui verrouille une famille.
			// L'interrupteur d'horodatage, lui, serait resté réglable ; la
			// granularité de la découverte est la famille, il la paie avec les
			// trois autres — compromis assumé et écrit au §8 du rapport.
			if cfg.channel_name.is_none() {
				portee.ecrivable = false;
				portee.motif = Some(MOTIF_SANS_OSDCHANNEL);
			}
			publish_osd(publ, &cfg).await;
			Ok(4)
		}
		.await,
		Family::Motion => async {
			let cfg = bc.get_motion_cfg().await?;
			// Deux porteurs, un seul réécrivable : `<MD>` se renvoie, `<MdAlarm>`
			// non (msg 47 attend un `<MD>`). Lire l'un des deux n'autorise donc
			// pas à promettre l'écriture — c'est exactement ce que la découverte
			// ne doit plus faire.
			portee.ecrivable = matches!(cfg, MotionCfg::Md(_));
			publish_motion(publ, &cfg).await;
			Ok(2)
		}
		.await,
		Family::Pir => async {
			let cfg = bc.get_pirstate().await?;
			publish_pir(publ, &cfg).await;
			Ok(2)
		}
		.await,
		Family::Ntp => async {
			let cfg = bc.get_ntp().await?;
			publish_ntp(publ, &cfg).await;
			Ok(3)
		}
		.await,
		Family::Dst => async {
			let cfg = bc.get_dst().await?;
			publish_dst(publ, &cfg).await;
			Ok(2)
		}
		.await,
		Family::AutoReboot => async {
			let cfg = bc.get_auto_reboot().await?;
			publish_auto_reboot(publ, &cfg).await;
			Ok(4)
		}
		.await,
		Family::General => async {
			let cfg = bc.get_general().await?;
			publish_general(publ, &cfg).await;
			Ok(5)
		}
		.await,
		Family::Mask => async {
			let cfg = bc.get_privacy_masks().await?;
			publish_masks(publ, &cfg).await;
			Ok(2)
		}
		.await,
		Family::Sleep => async {
			let cfg = bc.get_sleep_state().await?;
			publish_sleep(publ, &cfg).await;
			Ok(1)
		}
		.await,
		Family::Wifi => async {
			let cfg = bc.get_wifi_signal().await?;
			publish_wifi(publ, &cfg).await;
			Ok(1)
		}
		.await,
		Family::Battery => async {
			let cfg = bc.battery_info().await?;
			publish_battery(publ, &cfg).await;
			Ok(5)
		}
		.await,
		Family::Chime => async {
			// Cinq commandes (483, 484, 485, 486, 609) : la famille la plus chère
			// du protocole, et la seule dont la lecture se refuse d'elle-même à un
			// modèle sans carillon.
			let cfg = bc.get_ding_dong_cfg().await?;
			publish_chime(publ, &cfg).await;
			// Pas d'identifiant = rien où visser l'`<id>` du 485 : le volume
			// n'est pas « refusé par la caméra », il est **inadressable**, et
			// l'écrire serait envoyer une commande sans destinataire. Verdict
			// local, comme `<MdAlarm>` et `<OsdChannelName>` — pas un 400.
			if cfg.chime_id.is_none() {
				portee.ecrivable = false;
				portee.motif = Some(if cfg.list_answered_without_entry {
					MOTIF_CARILLON_SANS_ENTREE
				} else {
					MOTIF_CARILLON_SANS_ID
				});
			}
			Ok(4)
		}
		.await,
		Family::Ai => async {
			// Aucune commande caméra : c'est le verdict, pas une relecture.
			//
			// Les entités `ai/*` publient leur état (elles viennent de la
			// poussée 33) et n'ont **pas** de topic de commande. La cause n'est
			// pas dans le pont : aucune balise d'activation d'un type n'a jamais
			// été lue nulle part (`reglages-app-reolink.md` 55-58 ne documente
			// que `<sensitivity>`/`<stayTime>`/tailles sur 342/343, et 342 se
			// interroge **par type**). Publier un `command_topic` serait promettre
			// une écriture sur une balise inventée. D'où ce motif, qui remplace le
			// silence par la vérification à faire ; et d'où l'absence d'état
			// touché ici — les états des capteurs d'événements ne se relisent pas.
			portee.ecrivable = false;
			portee.motif = Some(MOTIF_AI_SANS_BALISE);
			Ok(0)
		}
		.await,
	}
}

/// Synchronisation complète, dans l'ordre de [`Family::ALL`] (la batterie d'abord :
/// c'est la trame 253, celle qui ne réveille pas la caméra et dont l'opératrice a
/// le plus souvent besoin).
///
/// **Jamais périodique.** Sur une sonnette sur batterie, chaque famille est une
/// commande radio ; cette fonction n'est appelée que sur `query/reglages`, donc
/// sur demande humaine ou automatisation explicite.
///
/// Le retour — un `Vec` que l'appelant fournit et qui se remplit au fil de l'eau
/// — énumère **toutes** les familles dans l'ordre lu, chacune avec son verdict
/// ([`Lecture`]) : c'est ce dont la découverte a besoin pour publier les réglages
/// modifiables, en lecture seule, ou les laisser où ils sont. Une famille refusée
/// d'un code passager y figure en [`Lecture::Transitoire`] et se publie
/// **modifiable** avec sa raison à côté ; une famille qu'on n'a pas pu tenter du
/// tout (pas de session, caméra injoignable) y figure en [`Lecture::Indetermine`]
/// et **ne change pas de statut**. Les confondre, c'était la régression du
/// premier jour de terrain : quatre familles verrouillées par un `400` pendant que
/// la caméra se réveillait.
///
/// Le `Vec` est un paramètre et non une valeur de retour : quand la
/// synchronisation complète est interrompue par le délai du dispatch, les
/// familles déjà relues avant l'interruption sont quand même publiées par la
/// découverte. Les jeter avec le reste laisserait Home Assistant promettre une
/// écriture sur une famille qu'on vient justement de relire.
pub(crate) async fn sync_all(
	bc: &BcCamera,
	publ: &StatusPublisher<'_>,
	lues: &mut Vec<(Family, Lecture)>,
) {
	lues.reserve(Family::ALL.len());
	for family in Family::ALL {
		lues.push(lire_famille(bc, publ, family).await);
	}
}

// ── Écriture ────────────────────────────────────────────────────────────

/// La caméra a imprimé cette balise, on peut donc la modifier. Sinon, refuser :
/// écrire `<tag>` dans un bloc qui n'en contient pas, c'est envoyer au firmware
/// une balise qu'il n'a jamais déclarée — exactement l'écriture qu'il avale sans
/// rien dire.
fn tag_present(current: Option<&str>, tag: &str) -> CoreResult<()> {
	current.map(|_| ()).ok_or_else(|| {
		Error::Usage(format!(
			"this camera's reply has no <{tag}> tag, so this setting cannot be written; \
			 refusing to invent a tag the firmware did not send"
		))
	})
}

/// Idem pour une balise numérique (`<enable>`, `<offset>`), dont l'absence se
/// voit aussi bien sur l'`Option`.
fn tag_present_opt<T>(current: &Option<T>, tag: &str) -> CoreResult<()> {
	current
		.as_ref()
		.map(|_| ())
		.ok_or_else(|| {
			Error::Usage(format!(
				"this camera's reply has no <{tag}> tag, so this setting cannot be written; \
				 refusing to invent a tag the firmware did not send"
			))
		})
}

/// Applique une commande de réglage : lecture du bloc, retouche de la seule
/// balise demandée, écriture, relecture, publication de la relecture.
///
/// Le `Err` remonte tel quel au dispatch, qui répond `FAIL` sur le topic de
/// commande : c'est ce qui distingue une écriture refusée d'une écriture
/// acceptée puis ignorée (dans ce dernier cas la valeur publiée n'est pas celle
/// qui a été demandée, et le journal le dit).
pub(crate) async fn apply_setting(
	bc: &BcCamera,
	publ: &StatusPublisher<'_>,
	cmd: &SettingCommand,
) -> Result {
	match cmd {
		// ── audioCfg (264 / 265) ────────────────────────────────────────
		// Trois balises, trois entités : `<volume>` (général),
		// `<talkAndReplyVolume>` (parole), `<visitorVolume>` (visiteur). Chacune
		// se relit, se retouche seule, se réécrit, se relit.
		SettingCommand::AudioGeneralVolume(v) => {
			let mut cfg = bc.get_audio_cfg(false).await?;
			// `AudioCfg::volume` est un `String` et non un `Option` : le modèle
			// 264 imprime la balise, un firmware qui ne la connaît pas laisse la
			// chaîne vide. Y écrire quand même enverrait au firmware une balise
			// qu'il n'a jamais déclarée — refuser est le bon geste.
			tag_present(
				Some(cfg.volume.as_str()).filter(|s| !s.trim().is_empty()),
				"volume",
			)?;
			cfg.volume = v.clone();
			let after = bc.set_audio_cfg(&cfg, false).await?;
			warn_if_ignored("volume", v, Some(after.volume.as_str()));
			publish_audio(publ, &after).await;
		}
		SettingCommand::AudioVolume(v) => {
			let mut cfg = bc.get_audio_cfg(false).await?;
			tag_present(cfg.talk_and_reply_volume.as_deref(), "talkAndReplyVolume")?;
			cfg.talk_and_reply_volume = Some(v.clone());
			let after = bc.set_audio_cfg(&cfg, false).await?;
			warn_if_ignored("talkAndReplyVolume", v, after.talk_and_reply_volume.as_deref());
			publish_audio(publ, &after).await;
		}
		SettingCommand::AudioVisitorVolume(v) => {
			let mut cfg = bc.get_audio_cfg(false).await?;
			tag_present(cfg.visitor_volume.as_deref(), "visitorVolume")?;
			cfg.visitor_volume = Some(v.clone());
			let after = bc.set_audio_cfg(&cfg, false).await?;
			warn_if_ignored("visitorVolume", v, after.visitor_volume.as_deref());
			publish_audio(publ, &after).await;
		}
		SettingCommand::AudioVisitorLoudspeaker(on) => {
			let mut cfg = bc.get_audio_cfg(false).await?;
			tag_present(cfg.visitor_loudspeaker.as_deref(), "visitorLoudspeaker")?;
			let text = bit(*on);
			cfg.visitor_loudspeaker = Some(text.to_string());
			let after = bc.set_audio_cfg(&cfg, false).await?;
			warn_if_ignored("visitorLoudspeaker", text, after.visitor_loudspeaker.as_deref());
			publish_audio(publ, &after).await;
		}
		// ── aiDenoise (439 / 440) ───────────────────────────────────────
		SettingCommand::DenoiseEnable(on) => {
			let mut cfg = bc.get_ai_denoise().await?;
			tag_present(cfg.enable.as_deref(), "enable")?;
			cfg.enable = Some(bit(*on).to_string());
			let after = bc.set_ai_denoise(&cfg).await?;
			warn_if_ignored("aiDenoise.enable", bit(*on), after.enable.as_deref());
			publish_denoise(publ, &after).await;
		}
		SettingCommand::DenoiseLevel(v) => {
			let mut cfg = bc.get_ai_denoise().await?;
			tag_present(cfg.level.as_deref(), "level")?;
			cfg.level = Some(v.clone());
			let after = bc.set_ai_denoise(&cfg).await?;
			warn_if_ignored("aiDenoise.level", v, after.level.as_deref());
			publish_denoise(publ, &after).await;
		}
		// ── image (26 / 25) ─────────────────────────────────────────────
		SettingCommand::ImageBright(v) => {
			let mut cfg = bc.get_image_cfg().await?;
			tag_present(cfg.video_input.bright.as_deref(), "bright")?;
			cfg.video_input.bright = Some(v.clone());
			publish_image(publ, &bc.set_image_cfg(&cfg).await?).await;
		}
		SettingCommand::ImageContrast(v) => {
			let mut cfg = bc.get_image_cfg().await?;
			tag_present(cfg.video_input.contrast.as_deref(), "contrast")?;
			cfg.video_input.contrast = Some(v.clone());
			publish_image(publ, &bc.set_image_cfg(&cfg).await?).await;
		}
		SettingCommand::ImageSaturation(v) => {
			let mut cfg = bc.get_image_cfg().await?;
			tag_present(cfg.video_input.saturation.as_deref(), "saturation")?;
			cfg.video_input.saturation = Some(v.clone());
			publish_image(publ, &bc.set_image_cfg(&cfg).await?).await;
		}
		SettingCommand::ImageHue(v) => {
			let mut cfg = bc.get_image_cfg().await?;
			tag_present(cfg.video_input.hue.as_deref(), "hue")?;
			cfg.video_input.hue = Some(v.clone());
			publish_image(publ, &bc.set_image_cfg(&cfg).await?).await;
		}
		SettingCommand::ImageSharpen(v) => {
			let mut cfg = bc.get_image_cfg().await?;
			tag_present(cfg.video_input.sharpen.as_deref(), "sharpen")?;
			cfg.video_input.sharpen = Some(v.clone());
			publish_image(publ, &bc.set_image_cfg(&cfg).await?).await;
		}
		SettingCommand::ImageDayNight(mode) => {
			let mut cfg = bc.get_image_cfg().await?;
			tag_present(cfg.video_input.day_night.as_deref(), "dayNight")?;
			cfg.video_input.day_night = Some(mode.as_text().to_string());
			publish_image(publ, &bc.set_image_cfg(&cfg).await?).await;
		}
		// ── osd (44 / 45) ───────────────────────────────────────────────
		SettingCommand::OsdDatetimeEnable(on) => {
			let mut cfg = bc.get_osd_cfg().await?;
			tag_present(cfg.datetime.enable.as_deref(), "enable")?;
			cfg.datetime.enable = Some(bit(*on).to_string());
			publish_osd(publ, &bc.set_osd_cfg(&cfg).await?).await;
		}
		SettingCommand::OsdNameEnable(on) => {
			let mut cfg = bc.get_osd_cfg().await?;
			let mut block = cfg.channel_name.clone().ok_or_else(|| {
				Error::Usage(
					"this camera's osd reply has no <OsdChannelName> block, so the channel \
					 name cannot be written"
						.to_string(),
				)
			})?;
			tag_present(block.enable.as_deref(), "enable")?;
			block.enable = Some(bit(*on).to_string());
			cfg.channel_name = Some(block);
			publish_osd(publ, &bc.set_osd_cfg(&cfg).await?).await;
		}
		SettingCommand::OsdName(name) => {
			let mut cfg = bc.get_osd_cfg().await?;
			let mut block = cfg.channel_name.clone().ok_or_else(|| {
				Error::Usage(
					"this camera's osd reply has no <OsdChannelName> block, so the channel \
					 name cannot be written"
						.to_string(),
				)
			})?;
			tag_present(block.name.as_deref(), "name")?;
			block.name = Some(name.clone());
			cfg.channel_name = Some(block);
			publish_osd(publ, &bc.set_osd_cfg(&cfg).await?).await;
		}
		SettingCommand::OsdWatermark(on) => {
			let mut cfg = bc.get_osd_cfg().await?;
			let mut block = cfg.channel_name.clone().ok_or_else(|| {
				Error::Usage(
					"this camera's osd reply has no <OsdChannelName> block, so the watermark \
					 switch cannot be written"
						.to_string(),
				)
			})?;
			tag_present(block.en_watermark.as_deref(), "enWatermark")?;
			block.en_watermark = Some(bit(*on).to_string());
			cfg.channel_name = Some(block);
			publish_osd(publ, &bc.set_osd_cfg(&cfg).await?).await;
		}
		// ── mouvement (46 / 47) ─────────────────────────────────────────
		SettingCommand::MotionEnable(on) => {
			let mut cfg = bc.get_motion_cfg().await?;
			tag_present(cfg.enable(), "enable")?;
			cfg.set_enable(bit(*on));
			publish_motion(publ, &bc.set_motion_cfg(&cfg).await?).await;
		}
		SettingCommand::MotionSensitivity(v) => {
			let mut cfg = bc.get_motion_cfg().await?;
			tag_present(cfg.sensitivity(), "sensitivity")?;
			cfg.set_sensitivity(v.clone());
			publish_motion(publ, &bc.set_motion_cfg(&cfg).await?).await;
		}
		// ── PIR : la sensibilité seulement (l'interrupteur reste control/pir)
		SettingCommand::PirSensitivity(v) => {
			let mut cfg = bc.get_pirstate().await?;
			// Les deux orthographes selon le firmware ; on retouche celle qui est
			// là, et on refuse si aucune ne l'est.
			if cfg.sensiValue.is_some() {
				tag_present_opt(&cfg.sensiValue, "sensiValue")?;
				cfg.sensiValue = Some(v.parse::<u8>().map_err(|_| {
					Error::Usage(format!("<sensiValue> must be an 8-bit integer, got {v}"))
				})?);
			} else if cfg.sensitivity.is_some() {
				tag_present_opt(&cfg.sensitivity, "sensitivity")?;
				cfg.sensitivity = Some(v.parse::<u8>().map_err(|_| {
					Error::Usage(format!("<sensitivity> must be an 8-bit integer, got {v}"))
				})?);
			} else {
				return Err(Error::Usage(
					"this camera's PIR reply has neither <sensiValue> nor <sensitivity>, so \
					 the sensitivity cannot be written"
						.to_string(),
				));
			}
			bc.set_pirstate(cfg).await?;
			// Relecture : c'est ce que le matériel a gardé qui est publié.
			let after = bc.get_pirstate().await?;
			publish_pir(publ, &after).await;
		}
		// ── PIR : le temps de repos entre deux alarmes (même porteur, 212/213)
		SettingCommand::PirInterval(v) => {
			let mut cfg = bc.get_pirstate().await?;
			// `<interval>` n'existe que sur le firmware v1.1+. Une caméra qui ne
			// l'a jamais imprimé ne reçoit **rien** : le pont n'invente pas au
			// firmware une balise qu'il n'a pas déclarée, et le motif publié sous
			// `status/pir/reason` le dit à la place d'un échec muet.
			tag_present_opt(&cfg.interval, "interval")?;
			cfg.interval = Some(v.parse::<u32>().map_err(|_| {
				Error::Usage(format!("<interval> must be a whole number of seconds, got {v}"))
			})?);
			bc.set_pirstate(cfg).await?;
			let after = bc.get_pirstate().await?;
			publish_pir(publ, &after).await;
			warn_if_ignored(
				"interval",
				&v,
				after.interval.map(|g| g.to_string()).as_deref(),
			);
		}
		// ── ntp (38 / 39) ───────────────────────────────────────────────
		SettingCommand::NtpEnable(on) => {
			let mut cfg = bc.get_ntp().await?;
			tag_present(cfg.enable.as_deref(), "enable")?;
			cfg.enable = Some(bit(*on).to_string());
			publish_ntp(publ, &bc.set_ntp(&cfg).await?).await;
		}
		SettingCommand::NtpServer(host) => {
			let mut cfg = bc.get_ntp().await?;
			tag_present(cfg.server.as_deref(), "server")?;
			cfg.server = Some(host.clone());
			publish_ntp(publ, &bc.set_ntp(&cfg).await?).await;
		}
		SettingCommand::NtpInterval(v) => {
			let mut cfg = bc.get_ntp().await?;
			tag_present(cfg.synchronize_interval.as_deref(), "synchronizeInterval")?;
			cfg.synchronize_interval = Some(v.clone());
			publish_ntp(publ, &bc.set_ntp(&cfg).await?).await;
		}
		// ── heure d'été (106 / 107) ─────────────────────────────────────
		SettingCommand::DstEnable(on) => {
			let mut cfg = bc.get_dst().await?;
			tag_present_opt(&cfg.enable, "enable")?;
			cfg.enable = Some(u32::from(*on));
			publish_dst(publ, &bc.set_dst(&cfg).await?).await;
		}
		SettingCommand::DstOffset(v) => {
			let mut cfg = bc.get_dst().await?;
			tag_present_opt(&cfg.offset, "offset")?;
			cfg.offset = Some(v.parse::<i32>().map_err(|_| {
				Error::Usage(format!("<offset> must be a whole number of hours, got {v}"))
			})?);
			publish_dst(publ, &bc.set_dst(&cfg).await?).await;
		}
		// ── redémarrage automatique (101 / 100) ─────────────────────────
		SettingCommand::AutoRebootEnable(on) => {
			let mut cfg = bc.get_auto_reboot().await?;
			tag_present(cfg.enable.as_deref(), "enable")?;
			cfg.enable = Some(bit(*on).to_string());
			publish_auto_reboot(publ, &bc.set_auto_reboot(&cfg).await?).await;
		}
		SettingCommand::AutoRebootWeekday(d) => {
			let mut cfg = bc.get_auto_reboot().await?;
			tag_present(cfg.week_day.as_deref(), "weekDay")?;
			cfg.week_day = Some(d.as_text().to_string());
			publish_auto_reboot(publ, &bc.set_auto_reboot(&cfg).await?).await;
		}
		SettingCommand::AutoRebootHour(v) => {
			let mut cfg = bc.get_auto_reboot().await?;
			tag_present(cfg.hour.as_deref(), "hour")?;
			cfg.hour = Some(v.clone());
			publish_auto_reboot(publ, &bc.set_auto_reboot(&cfg).await?).await;
		}
		SettingCommand::AutoRebootMinute(v) => {
			let mut cfg = bc.get_auto_reboot().await?;
			tag_present(cfg.minute.as_deref(), "minute")?;
			cfg.minute = Some(v.clone());
			publish_auto_reboot(publ, &bc.set_auto_reboot(&cfg).await?).await;
		}
		// ── système (104 / 105) ─────────────────────────────────────────
		SettingCommand::GeneralOsdFormat(f) => {
			let mut cfg = bc.get_general().await?;
			tag_present(cfg.osd_format.as_deref(), "osdFormat")?;
			cfg.osd_format = Some(f.as_text().to_string());
			publish_general(publ, &bc.set_general(&cfg).await?).await;
		}
		SettingCommand::GeneralTimeFormat(v) => {
			let mut cfg = bc.get_general().await?;
			tag_present_opt(&cfg.time_format, "timeFormat")?;
			cfg.time_format = Some(v.parse::<u8>().map_err(|_| {
				Error::Usage(format!("<timeFormat> must be 0 or 1, got {v}"))
			})?);
			publish_general(publ, &bc.set_general(&cfg).await?).await;
		}
		SettingCommand::GeneralLanguage(v) => {
			let mut cfg = bc.get_general().await?;
			tag_present(cfg.language.as_deref(), "language")?;
			cfg.language = Some(v.clone());
			publish_general(publ, &bc.set_general(&cfg).await?).await;
		}
		SettingCommand::GeneralDeviceName(v) => {
			let mut cfg = bc.get_general().await?;
			tag_present(cfg.device_name.as_deref(), "deviceName")?;
			cfg.device_name = Some(v.clone());
			publish_general(publ, &bc.set_general(&cfg).await?).await;
		}
		// ── carillon ────────────────────────────────────────────────────
		SettingCommand::ChimeRing { chime_id, music_id } => {
			// Sans id explicite, on sonne le carillon que la caméra préfère
			// (celui que `get_ding_dong_cfg` a retenu). Le refus est explicite :
			// « aucun carillon appairé » n'est pas un succès muet.
			let id = match chime_id.as_deref() {
				Some(id) => id.to_string(),
				None => {
					let cfg = Box::pin(bc.get_ding_dong_cfg()).await?;
					cfg.chime_id.ok_or_else(|| {
						Error::Usage(
							"no paired wireless chime: nothing to ring (the camera listed none)"
								.to_string(),
						)
					})?
				}
			};
			bc.ring_ding_dong(&id, music_id).await?;
			// Sonner n'est pas un état : on ne relit pas la liste du carillon
			// pour ça, ce seraient cinq commandes radio pour rien.
		}
		SettingCommand::ChimeVolume(v) => {
			// Le volume se renvoie en 485 `setParam`, donc il faut un carillon
			// **adressable** et une balise `<volLevel>` que la caméra a imprimée.
			// Les deux refus sont locaux et explicites : sans id, la commande
			// n'a pas de destinataire ; sans balise lue, l'écrire serait inventer
			// une balise — la règle de ce fichier, appliquée côté écriture.
			let mut cfg = Box::pin(bc.get_ding_dong_cfg()).await?;
			cfg.chime_id.clone().ok_or_else(|| {
				Error::Usage(
					"no addressable wireless chime: nothing to set the volume of (the camera listed none)"
						.to_string(),
				)
			})?;
			let params = cfg.params.as_mut().ok_or_else(|| {
				Error::Usage(
					"the chime answered 485 getParam without a block: <volLevel> was never printed"
						.to_string(),
				)
			})?;
			tag_present(params.vol_level.as_deref(), "volLevel")?;
			params.vol_level = Some(v.clone());
			let lu = bc.set_ding_dong_cfg(&cfg).await?;
			// C'est la relecture qui est publiée, comme pour chaque autre feuille.
			publish_chime(publ, &lu).await;
			warn_if_ignored(
				"volLevel",
				&v,
				lu.params.as_ref().and_then(|p| p.vol_level.as_deref()),
			);
		}
	}
	Ok(())
}

/// `1`/`0`, le texte que les firmware attendent pour un drapeau.
fn bit(on: bool) -> &'static str {
	if on {
		"1"
	} else {
		"0"
	}
}

/// Le firmware a répondu 200 puis gardé une autre valeur : c'est le cas
/// « écriture acceptée, ignorée » que le cache de la valeur demandée a toujours
/// masqué. Le journal le dit, l'état publié aussi (c'est la relecture).
fn warn_if_ignored(tag: &str, requested: &str, kept: Option<&str>) {
	if kept != Some(requested) {
		tracing::warn!(
			tag,
			requested,
			kept = kept.unwrap_or("<absent>"),
			"la caméra a accepté l'écriture puis gardé une autre valeur : c'est elle qui est publiée"
		);
	}
}

#[cfg(test)]
mod tests {
	use bairelay_neolink_core::bc::xml::{BcXml, DingDongCfg, SystemGeneral};
	use bairelay_neolink_core::bc_protocol::ImageCfg;

	/// La taille de [`BcXml`] est la contrainte de pile cachée du pont. Chaque
	/// commande caméra brasse au moins un de ces blocs — souvent deux quand une
	/// écriture est suivie d'une relecture — et le bloc se déplace par valeur.
	/// Les réglages ajoutés ici remplissent davantage ses variantes : le poids
	/// par bloc est le même que celui des blocs déjà là, mais la chaîne
	/// `dispatch_control → famille → commande` est plus longue qu'avant.
	///
	/// Ce test est un réveil, pas un diplôme : si le bloc grossit encore, la
	/// question « tient-on dans les 2 Mo d'un worker tokio ? » doit être reposée
	/// — et ce sont les tests de `crate::mqtt_dispatch`, passés en release, qui
	/// y répondent. Les chiffres sont imprimés pour le rapport.
	#[test]
	fn bcxml_garde_une_taille_connue() {
		let blocs = [
			("BcXml", core::mem::size_of::<BcXml>()),
			("ImageCfg", core::mem::size_of::<ImageCfg>()),
			("DingDongCfg", core::mem::size_of::<DingDongCfg>()),
			("SystemGeneral", core::mem::size_of::<SystemGeneral>()),
		];
		for (nom, taille) in blocs {
			eprintln!("size_of::<{nom}>() = {taille} octets");
		}
		assert!(
			core::mem::size_of::<BcXml>() < 96 * 1024,
			"BcXml grossit au-delà de ce que le pont sait empiler ; ce point de dialogue est un signal faible, pas un cap : le seuil est relevé à 96 KiB pour absorber les blocs de réglages, et un dépassement doit déclencher la discussion sur la pile des workers tokio"
		);
	}

	/// Le motif d'un refus de **lecture** porte la réponse de la caméra, et
	/// reste publiable tel quel : une ligne, pas de balise, borné.
	///
	/// C'est la mesure du 13/11 : quatre familles de cette sonnette meurent d'un
	/// `400` et le broker ne disait que « code 400 ». Ce que la caméra écrit sous
	/// son code est la seule chose qui distingue une forme de requête mauvaise
	/// d'une famille absente du micrologiciel — donc ça doit arriver jusqu'à
	/// l'opératrice, et pas s'arrêter dans le journal d'un conteneur.
	#[test]
	fn le_motif_dun_refus_de_lecture_porte_la_reponse() {
		let err = bairelay_neolink_core::Error::CameraRefused {
			id: 26,
			code: 400,
			detail: "<error><rspCode>-4</rspCode>\n<rspString>cmd not support</rspString></error>"
				.to_string(),
		};
		let motif = super::motif(&err);
		assert!(motif.contains("400"), "le code reste dans la phrase : {motif}");
		assert!(
			motif.contains("cmd not support"),
			"la caméra doit être lisible dans le motif : {motif}"
		);

		let publie = bairelay_mqtt::status::raison_publiable(&motif);
		assert!(!publie.contains('<'), "aucune balise ne part sur le broker : {publie}");
		assert!(
			publie.chars().count() <= 160,
			"la ligne reste bornée : {} caractères",
			publie.chars().count()
		);
		assert!(
			publie.contains("cmd not support"),
			"la coupe ne doit pas manger la réponse de la caméra : {publie}"
		);
	}
}
