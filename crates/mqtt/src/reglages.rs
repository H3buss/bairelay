//! Parsing des commandes de réglages arrivant sur MQTT.
//!
//! Le topic est `{prefix}/{cam}/control/{famille}/{feuille}` et le payload la
//! valeur. Une seule chose est ajoutée au-delà du décodage : **la validation
//! des bornes du protocole**, ici et pas plus loin. Une valeur hors plage ne
//! part jamais vers la caméra, pour trois raisons :
//!
//! * certaines firmwares acceptent la requête (200) puis stockent n'importe
//!   quoi — un `OK` mensonger ;
//! * d'autres éteignent silencieusement le réglage concerné ;
//! * et surtout, `docs/protocole-reglages.md` §1 : un bloc partiellement
//!   invalide peut être annulé **sans réponse d'erreur**.
//!
//! Le refus se traduit par un `FAIL` sur le même topic control, ce que Home
//! Assistant affiche ; le réglage, lui, n'a pas bougé.
//!
//! Les plages viennent du document, pas de l'inspiration : 0-100 pour les
//! volumes `<audioCfg>`, 1-100 pour `<aiDenoise><level>` (un `level <= 0`
//! éteint le débruiteur, ce n'est pas une valeur qu'on envoie au hasard),
//! 0-255 pour les curseurs image, 0-50 pour la sensibilité de détection,
//! 1-1440 minutes pour la synchro NTP.

use std::str;

/// Les familles de réglages, telles qu'elles apparaissent dans les topics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
	/// `<audioCfg>` (264/265) : interphone.
	Audio,
	/// `<aiDenoise>` (439/440) : débruiteur micro.
	Denoise,
	/// `<VideoInput>` + `<InputAdvanceCfg>` (26/25).
	Image,
	/// `<OsdDatetime>` + `<OsdChannelName>` (44/45).
	Osd,
	/// `<MD>` / `<MdAlarm>` (46/47).
	Motion,
	/// `<RfAlarmCfg>` (212/213).
	Pir,
	/// `<Ntp>` (38/39).
	Ntp,
	/// `<Dst>` (106/107).
	Dst,
	/// `<AutoReboot>` (101/100).
	AutoReboot,
	/// `<SystemGeneral>` (104/105).
	General,
	/// Le carillon (483-487, 609, 610).
	Chime,
	/// `<Shelter>` (52) — lecture seule.
	Mask,
	/// `<sleepState>` (574) — lecture seule.
	Sleep,
	/// `<WifiSignal>` (115) — lecture seule.
	Wifi,
	/// `<BatteryInfo>` (253) — lecture seule, et non réveillante.
	Battery,
	/// Les reconnaissances (`ai/*`) : la famille existe pour que ses entités
	/// aient un verdict et un motif publiés, **pas** pour être relue.
	///
	/// Rien ici ne correspond à une commande unique : l'état vient de la poussée
	/// 33, et les réglages par type (`<AiDetectCfg>`, 342/343) sont interrogés
	/// **un par type**, avec une balise d'activation que personne n'a jamais lue
	/// (`docs/reglages-app-reolink.md` lignes 55-58, `docs/protocole-reglages.md`
	/// §4.2). La famille est donc épinglée en lecture seule tant que le dump 342
	/// n'existe pas — et c'est `status/ai/reason` qui le dit, au lieu du silence
	/// actuel.
	Ai,
}

impl Family {
	/// Le segment de topic.
	pub fn slug(self) -> &'static str {
		match self {
			Family::Audio => "audio",
			Family::Denoise => "denoise",
			Family::Image => "image",
			Family::Osd => "osd",
			Family::Motion => "motion",
			Family::Pir => "pir",
			Family::Ntp => "ntp",
			Family::Dst => "dst",
			Family::AutoReboot => "autoreboot",
			Family::General => "general",
			Family::Chime => "chime",
			Family::Mask => "mask",
			Family::Sleep => "sleep",
			Family::Wifi => "wifi",
			Family::Battery => "battery",
			Family::Ai => "ai",
		}
	}

	/// Le nom du topic de requête qui déclenche la (re)lecture de la famille.
	pub fn from_slug(slug: &str) -> Option<Self> {
		Some(match slug {
			"audio" => Family::Audio,
			"denoise" => Family::Denoise,
			"image" => Family::Image,
			"osd" => Family::Osd,
			"motion" => Family::Motion,
			"pir" => Family::Pir,
			"ntp" => Family::Ntp,
			"dst" => Family::Dst,
			"autoreboot" => Family::AutoReboot,
			"general" => Family::General,
			"chime" => Family::Chime,
			"mask" => Family::Mask,
			"sleep" => Family::Sleep,
			"wifi" => Family::Wifi,
			"battery" => Family::Battery,
			"ai" => Family::Ai,
			_ => return None,
		})
	}

	/// Toutes les familles, dans l'ordre où un `query/reglages` complet les
	/// relit.
	///
	/// `Ai` est **en dernier** et ne coûte aucune commande radio : c'est le
	/// verdict « rien n'est réglable tant que le 342 n'est pas dumpé » posé à la
	/// fin de la salve, pas une relecture.
	pub const ALL: [Family; 16] = [
		Family::Battery,
		Family::Audio,
		Family::Denoise,
		Family::Image,
		Family::Osd,
		Family::Motion,
		Family::Pir,
		Family::Ntp,
		Family::Dst,
		Family::AutoReboot,
		Family::General,
		Family::Mask,
		Family::Sleep,
		Family::Wifi,
		Family::Chime,
		Family::Ai,
	];
}

/// Une commande de réglage validée. Chaque variante porte sa valeur **telle
/// qu'il faut l'écrire dans le bloc**, c'est-à-dire en texte caméra : le
/// dispatch fait la lecture, patche cette feuille, et renvoie la relecture.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingCommand {
	// ── audioCfg ────────────────────────────────────────────────────────
	/// `<volume>`, 0-100 : le **volume général** du haut-parleur de la caméra.
	///
	/// `audioCfg` imprime trois volumes et le pont en découvrait deux. Cette
	/// feuille-là est celle qui manquait ; elle porte la balise `<volume>`, que
	/// le modèle 264 imprime en premier.
	AudioGeneralVolume(String),
	/// `<talkAndReplyVolume>`, 0-100 : le volume de la **parole** (interphone :
	/// ce que le visiteur entend pendant l'échange), pas le volume général.
	AudioVolume(String),
	/// `<visitorVolume>`, 0-100 : le volume du **visiteur** (ce que la caméra
	/// envoie dans le talkie).
	AudioVisitorVolume(String),
	/// `<visitorLoudspeaker>`, `"1"`/`"0"`.
	AudioVisitorLoudspeaker(bool),
	// ── aiDenoise ───────────────────────────────────────────────────────
	/// `<enable>`, `"1"`/`"0"`.
	DenoiseEnable(bool),
	/// `<level>`, 1-100.
	DenoiseLevel(String),
	// ── image ───────────────────────────────────────────────────────────
	ImageBright(String),
	ImageContrast(String),
	ImageSaturation(String),
	ImageHue(String),
	ImageSharpen(String),
	/// `<dayNight>`, l'un des textes que la caméra normalise.
	ImageDayNight(DayNight),
	// ── osd ─────────────────────────────────────────────────────────────
	OsdDatetimeEnable(bool),
	OsdNameEnable(bool),
	/// `<OsdChannelName><name>` : ce qui s'affiche sur l'image.
	OsdName(String),
	/// `<enWatermark>`, `"1"`/`"0"`.
	OsdWatermark(bool),
	// ── motion ──────────────────────────────────────────────────────────
	MotionEnable(bool),
	/// `<sensitivityDefault>`, 0-50, plus haut = plus sensible.
	MotionSensitivity(String),
	// ── pir ─────────────────────────────────────────────────────────────
	/// `<sensiValue>`. L'interrupteur, lui, reste `control/pir`.
	PirSensitivity(String),
	/// `<interval>` de ce même `<RfAlarmCfg>` : secondes de repos entre deux
	/// alarmes PIR. Porté par la paire 212/213 déjà maîtrisée ; la plage 5-120
	/// est celle que l'application expose, et les bornes que le firmware annonce
	/// (`intervalSecMin`/`intervalSecMax`) se lisent dans `status/pir/json`.
	PirInterval(String),
	// ── ntp ─────────────────────────────────────────────────────────────
	NtpEnable(bool),
	NtpServer(String),
	/// `<synchronizeInterval>` en **minutes**, 1-1440.
	NtpInterval(String),
	// ── dst ─────────────────────────────────────────────────────────────
	DstEnable(bool),
	/// `<offset>` en heures, -14 à 14.
	DstOffset(String),
	// ── autoreboot ──────────────────────────────────────────────────────
	AutoRebootEnable(bool),
	AutoRebootWeekday(Weekday),
	AutoRebootHour(String),
	AutoRebootMinute(String),
	// ── general ─────────────────────────────────────────────────────────
	GeneralOsdFormat(OsdFormat),
	/// `<timeFormat>` : `"0"` = 24 h, `"1"` = 12 h.
	GeneralTimeFormat(String),
	GeneralLanguage(String),
	/// `<deviceName>` : le nom système, pas le nom à l'écran ni le nom MQTT.
	GeneralDeviceName(String),
	// ── chime ───────────────────────────────────────────────────────────
	/// `<volLevel>` du carillon sans fil, **0-4** : la borne vue dans la
	/// documentation de référence (« 0-4 typical »), pas une plage arrondie au
	/// pourcentage. La borne haute réelle du firmware est INCONNUE ; si le
	/// terrain en montre une plus haute, c'est ici qu'elle se change, avec la
	/// ligne du document qui la prouve.
	ChimeVolume(String),
	/// Faire sonner : mélodie `music_id` sur le carillon `chime_id` (ou sur
	/// celui appairé quand aucun id n'est donné).
	ChimeRing { chime_id: Option<String>, music_id: String },
}

/// `<dayNight>` normalisé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayNight {
	Auto,
	Color,
	BlackAndWhite,
}

impl DayNight {
	/// Le texte que la caméra attend.
	pub fn as_text(self) -> &'static str {
		match self {
			DayNight::Auto => "auto",
			DayNight::Color => "color",
			DayNight::BlackAndWhite => "blackAndWhite",
		}
	}
}

/// `<AutoReboot><weekDay>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weekday {
	Everyday,
	Sunday,
	Monday,
	Tuesday,
	Wednesday,
	Thursday,
	Friday,
	Saturday,
}

impl Weekday {
	/// Le texte que la caméra attend.
	pub fn as_text(self) -> &'static str {
		match self {
			Weekday::Everyday => "everyday",
			Weekday::Sunday => "Sunday",
			Weekday::Monday => "Monday",
			Weekday::Tuesday => "Tuesday",
			Weekday::Wednesday => "Wednesday",
			Weekday::Thursday => "Thursday",
			Weekday::Friday => "Friday",
			Weekday::Saturday => "Saturday",
		}
	}
}

/// `<SystemGeneral><osdFormat>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsdFormat {
	Dmy,
	Mdy,
	Ymd,
}

impl OsdFormat {
	/// Le texte que la caméra attend.
	pub fn as_text(self) -> &'static str {
		match self {
			OsdFormat::Dmy => "DMY",
			OsdFormat::Mdy => "MDY",
			OsdFormat::Ymd => "YMD",
		}
	}
}

impl SettingCommand {
	/// La feuille du topic d'où vient la commande. Sert à reconstruire le
	/// topic de réponse `OK`/`FAIL`.
	pub fn leaf(&self) -> &'static str {
		use SettingCommand::*;
		match self {
			AudioGeneralVolume(_) => "general_volume",
			AudioVolume(_) => "volume",
			AudioVisitorVolume(_) => "visitor_volume",
			AudioVisitorLoudspeaker(_) => "visitor_loudspeaker",
			DenoiseEnable(_) => "enable",
			DenoiseLevel(_) => "level",
			ImageBright(_) => "bright",
			ImageContrast(_) => "contrast",
			ImageSaturation(_) => "saturation",
			ImageHue(_) => "hue",
			ImageSharpen(_) => "sharpen",
			ImageDayNight(_) => "day_night",
			OsdDatetimeEnable(_) => "datetime_enable",
			OsdNameEnable(_) => "name_enable",
			OsdName(_) => "name",
			OsdWatermark(_) => "watermark",
			MotionEnable(_) => "enable",
			MotionSensitivity(_) => "sensitivity",
			PirSensitivity(_) => "sensitivity",
			PirInterval(_) => "interval",
			NtpEnable(_) => "enable",
			NtpServer(_) => "server",
			NtpInterval(_) => "interval",
			DstEnable(_) => "enable",
			DstOffset(_) => "offset",
			AutoRebootEnable(_) => "enable",
			AutoRebootWeekday(_) => "weekday",
			AutoRebootHour(_) => "hour",
			AutoRebootMinute(_) => "minute",
			GeneralOsdFormat(_) => "osd_format",
			GeneralTimeFormat(_) => "time_format",
			GeneralLanguage(_) => "language",
			GeneralDeviceName(_) => "device_name",
			ChimeVolume(_) => "volume",
			ChimeRing { .. } => "ring",
		}
	}

	/// La famille du topic d'où vient la commande.
	pub fn family(&self) -> Family {
		use SettingCommand::*;
		match self {
			AudioGeneralVolume(_) | AudioVolume(_) | AudioVisitorVolume(_)
			| AudioVisitorLoudspeaker(_) => Family::Audio,
			DenoiseEnable(_) | DenoiseLevel(_) => Family::Denoise,
			ImageBright(_)
			| ImageContrast(_)
			| ImageSaturation(_)
			| ImageHue(_)
			| ImageSharpen(_)
			| ImageDayNight(_) => Family::Image,
			OsdDatetimeEnable(_) | OsdNameEnable(_) | OsdName(_) | OsdWatermark(_) => Family::Osd,
			MotionEnable(_) | MotionSensitivity(_) => Family::Motion,
			PirSensitivity(_) | PirInterval(_) => Family::Pir,
			NtpEnable(_) | NtpServer(_) | NtpInterval(_) => Family::Ntp,
			DstEnable(_) | DstOffset(_) => Family::Dst,
			AutoRebootEnable(_) | AutoRebootWeekday(_) | AutoRebootHour(_)
			| AutoRebootMinute(_) => Family::AutoReboot,
			GeneralOsdFormat(_) | GeneralTimeFormat(_) | GeneralLanguage(_)
			| GeneralDeviceName(_) => Family::General,
			ChimeRing { .. } | ChimeVolume(_) => Family::Chime,
		}
	}

	/// Décode et valide `(famille, feuille, payload)`. `None` = commande
	/// inconnue ou valeur hors plage : le dispatch répondra `FAIL` sans avoir
	/// touché la caméra.
	pub fn parse(family: Family, leaf: &str, payload: &str) -> Option<Self> {
		use SettingCommand::*;
		let p = payload.trim();
		Some(match (family, leaf) {
			// Les trois volumes d'`audioCfg`, tous 0-100 mais trois balises
			// distinctes : `<volume>` (général), `<talkAndReplyVolume>` (parole)
			// et `<visitorVolume>` (visiteur). Les confondre écrit la bonne
			// valeur au mauvais haut-parleur.
			(Family::Audio, "general_volume") => AudioGeneralVolume(int_in(p, 0, 100)?),
			(Family::Audio, "volume") => AudioVolume(int_in(p, 0, 100)?),
			(Family::Audio, "visitor_volume") => AudioVisitorVolume(int_in(p, 0, 100)?),
			(Family::Audio, "visitor_loudspeaker") => AudioVisitorLoudspeaker(on_off(p)?),
			(Family::Denoise, "enable") => DenoiseEnable(on_off(p)?),
			// 1-100 : un level <= 0 éteint le débruiteur côté firmware, ce qui
			// n'est pas ce qu'on appelle « régler la force ».
			(Family::Denoise, "level") => DenoiseLevel(int_in(p, 1, 100)?),
			(Family::Image, "bright") => ImageBright(int_in(p, 0, 255)?),
			(Family::Image, "contrast") => ImageContrast(int_in(p, 0, 255)?),
			(Family::Image, "saturation") => ImageSaturation(int_in(p, 0, 255)?),
			(Family::Image, "hue") => ImageHue(int_in(p, 0, 255)?),
			(Family::Image, "sharpen") => ImageSharpen(int_in(p, 0, 255)?),
			(Family::Image, "day_night") => ImageDayNight(match p {
				"auto" => DayNight::Auto,
				"color" => DayNight::Color,
				"blackAndWhite" => DayNight::BlackAndWhite,
				_ => return None,
			}),
			(Family::Osd, "datetime_enable") => OsdDatetimeEnable(on_off(p)?),
			(Family::Osd, "name_enable") => OsdNameEnable(on_off(p)?),
			(Family::Osd, "name") => OsdName(text(p, 1, 31)?),
			(Family::Osd, "watermark") => OsdWatermark(on_off(p)?),
			(Family::Motion, "enable") => MotionEnable(on_off(p)?),
			(Family::Motion, "sensitivity") => MotionSensitivity(int_in(p, 0, 50)?),
			(Family::Pir, "sensitivity") => PirSensitivity(int_in(p, 0, 100)?),
			// 5-120 s : la plage que l'application Reolink expose pour ce
			// réglage (`reglages-app-reolink.md`, ligne 51). Le firmware peut en
			// annoncer de plus étroites dans `intervalSecMin`/`intervalSecMax` :
			// dans ce cas il garde ce qu'il accepte, et c'est la relecture qui est
			// publiée — pas la valeur demandée.
			(Family::Pir, "interval") => PirInterval(int_in(p, 5, 120)?),
			(Family::Ntp, "enable") => NtpEnable(on_off(p)?),
			(Family::Ntp, "server") => NtpServer(host(p, 63)?),
			(Family::Ntp, "interval") => NtpInterval(int_in(p, 1, 1440)?),
			(Family::Dst, "enable") => DstEnable(on_off(p)?),
			(Family::Dst, "offset") => DstOffset(int_in(p, -14, 14)?),
			(Family::AutoReboot, "enable") => AutoRebootEnable(on_off(p)?),
			(Family::AutoReboot, "weekday") => AutoRebootWeekday(match p {
				"everyday" => Weekday::Everyday,
				"Sunday" => Weekday::Sunday,
				"Monday" => Weekday::Monday,
				"Tuesday" => Weekday::Tuesday,
				"Wednesday" => Weekday::Wednesday,
				"Thursday" => Weekday::Thursday,
				"Friday" => Weekday::Friday,
				"Saturday" => Weekday::Saturday,
				_ => return None,
			}),
			(Family::AutoReboot, "hour") => AutoRebootHour(int_in(p, 0, 23)?),
			(Family::AutoReboot, "minute") => AutoRebootMinute(int_in(p, 0, 59)?),
			(Family::General, "osd_format") => GeneralOsdFormat(match p {
				"DMY" => OsdFormat::Dmy,
				"MDY" => OsdFormat::Mdy,
				"YMD" => OsdFormat::Ymd,
				_ => return None,
			}),
			(Family::General, "time_format") => GeneralTimeFormat(match p {
				"0" | "1" => p.to_string(),
				_ => return None,
			}),
			(Family::General, "language") => GeneralLanguage(text(p, 1, 16)?),
			(Family::General, "device_name") => GeneralDeviceName(text(p, 1, 31)?),
			// Le volume du carillon : la plage **vue** (0-4), pas un 0-100 qui
			// n'est que le réflexe « volume ». Une valeur hors de cette plage ne
			// part pas : le 485 `setParam` stocke ce qu'on lui donne et répond
			// 200, ce qui est exactement le « OK » mensonger que ce fichier
			// refuse depuis sa première ligne.
			(Family::Chime, "volume") => ChimeVolume(int_in(p, 0, 4)?),
			(Family::Chime, "ring") => {
				let (chime_id, music_id) = match p.split_once(' ') {
					Some((chime, music)) => (Some(numeric(chime)?), numeric(music)?),
					None => (None, numeric(p)?),
				};
				ChimeRing { chime_id, music_id }
			}
			_ => return None,
		})
	}
}

/// `(famille, feuille)` est-elle une commande écrite ? Utilisé par le test de
/// cohérence de la découverte : toute entité HA avec un `command_topic` doit
/// atterrir ici.
pub fn accepts(family_slug: &str, leaf: &str) -> bool {
	Family::from_slug(family_slug)
		.map(|f| SettingCommand::parse(f, leaf, sample_value(f, leaf)).is_some())
		.unwrap_or(false)
}

/// Une valeur qui passe forcément la validation de cette feuille, pour
/// [`accepts`]. Les familles sans commande répondent `""` (et `accepts`
/// renvoie alors `false`).
fn sample_value(family: Family, leaf: &str) -> &'static str {
	match (family, leaf) {
		// Les cas particuliers avant `(Family::Image, _)`, sinon un texte
		// comme `day_night` hériterait d'un échantillon numérique.
		(Family::Image, "day_night") => "auto",
		(Family::Audio, "general_volume")
		| (Family::Audio, "volume")
		| (Family::Audio, "visitor_volume")
		| (Family::Denoise, "level")
		| (Family::Image, _)
		| (Family::Pir, "sensitivity")
		| (Family::Pir, "interval")
		| (Family::Motion, "sensitivity") => "10",
		(Family::Ntp, "interval") => "1440",
		(Family::AutoReboot, "hour") => "3",
		(Family::AutoReboot, "minute") => "30",
		(Family::Dst, "offset") => "1",
		(Family::Ntp, "server") => "pool.ntp.org",
		(Family::Osd, "name") | (Family::General, "device_name") => "Porche",
		(Family::General, "language") => "French",
		(Family::General, "time_format") => "0",
		(Family::General, "osd_format") => "DMY",
		(Family::AutoReboot, "weekday") => "everyday",
		(Family::Chime, "ring") => "1",
		(Family::Chime, "volume") => "2",
		(_, leaf)
			if matches!(
				leaf,
				"enable" | "datetime_enable" | "name_enable" | "visitor_loudspeaker" | "watermark"
			) =>
		{
			"on"
		}
		_ => "",
	}
}

/// `"on"`/`"off"` (et `"1"`/`"0"`, et `"true"`/`"false"` — HA envoie les trois
/// selon la carte) → booléen.
fn on_off(payload: &str) -> Option<bool> {
	match payload {
		"on" | "1" | "true" => Some(true),
		"off" | "0" | "false" => Some(false),
		_ => None,
	}
}

/// Un entier dans `[lo, hi]`, rendu sous sa **forme texte caméra** : sans
/// signe superflu, sans zéro de remplissage. Un `"007"` accepté tel quel
/// repartirait tel quel et la caméra pourrait le relire autrement.
fn int_in(payload: &str, lo: i32, hi: i32) -> Option<String> {
	let v: i32 = payload.parse().ok()?;
	if v < lo || v > hi {
		return None;
	}
	Some(v.to_string())
}

/// Une chaîne imprimable bornée. Les retours de ligne sont exclus : ils
/// cassent la trame XML et ne peuvent pas venir d'un champ HA légitime.
fn text(payload: &str, min: usize, max: usize) -> Option<String> {
	let len = payload.chars().count();
	if len < min || len > max {
		return None;
	}
	if payload.chars().any(|c| c.is_control()) {
		return None;
	}
	Some(payload.to_string())
}

/// Un nom d'hôte ou une adresse IP de serveur NTP : pas d'espaces, pas de
/// pointeur d'URL, borné.
fn host(payload: &str, max: usize) -> Option<String> {
	if !payload.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
	{
		return None;
	}
	text(payload, 1, max)
}

/// Un identifiant purement numérique (id de carillon ou de mélodie).
fn numeric(payload: &str) -> Option<String> {
	if payload.is_empty() || !payload.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	Some(payload.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn les_familles_ronde_tripent_en_slugs() {
		for f in Family::ALL {
			assert_eq!(Family::from_slug(f.slug()), Some(f), "slug {}", f.slug());
		}
		assert_eq!(Family::from_slug("inconnu"), None);
	}

	#[test]
	fn un_volume_hors_plage_ne_part_pas() {
		assert_eq!(
			SettingCommand::parse(Family::Audio, "volume", "100"),
			Some(SettingCommand::AudioVolume("100".into()))
		);
		assert_eq!(SettingCommand::parse(Family::Audio, "volume", "101"), None);
		assert_eq!(SettingCommand::parse(Family::Audio, "volume", "-1"), None);
		assert_eq!(SettingCommand::parse(Family::Audio, "volume", "12.5"), None);
		assert_eq!(SettingCommand::parse(Family::Audio, "volume", ""), None);
	}

	#[test]
	fn le_debruiteur_refuse_zero_et_accepte_cent() {
		// `level <= 0` éteint le débruiteur côté firmware : ce n'est pas une
		// valeur de réglage, c'est un interrupteur déguisé.
		assert_eq!(SettingCommand::parse(Family::Denoise, "level", "0"), None);
		assert_eq!(
			SettingCommand::parse(Family::Denoise, "level", "1"),
			Some(SettingCommand::DenoiseLevel("1".into()))
		);
		assert_eq!(
			SettingCommand::parse(Family::Denoise, "level", "100"),
			Some(SettingCommand::DenoiseLevel("100".into()))
		);
		assert_eq!(SettingCommand::parse(Family::Denoise, "level", "101"), None);
	}

	#[test]
	fn la_sensibilite_mouvement_est_bornee_a_cinquante() {
		assert!(SettingCommand::parse(Family::Motion, "sensitivity", "50").is_some());
		assert_eq!(SettingCommand::parse(Family::Motion, "sensitivity", "51"), None);
	}

	#[test]
	fn les_interrupteurs_acceptent_les_trois_orthographes_ha() {
		for yes in ["on", "1", "true"] {
			assert_eq!(
				SettingCommand::parse(Family::Ntp, "enable", yes),
				Some(SettingCommand::NtpEnable(true)),
				"{yes}"
			);
		}
		for no in ["off", "0", "false"] {
			assert_eq!(
				SettingCommand::parse(Family::Ntp, "enable", no),
				Some(SettingCommand::NtpEnable(false)),
				"{no}"
			);
		}
		assert_eq!(SettingCommand::parse(Family::Ntp, "enable", "peut-etre"), None);
	}

	#[test]
	fn les_zeros_de_remplissage_sont_normalises() {
		assert_eq!(
			SettingCommand::parse(Family::Image, "bright", "007"),
			Some(SettingCommand::ImageBright("7".into())),
			"la caméra relit ce qu'elle stocke : on n'envoie que la forme canonique"
		);
	}

	#[test]
	fn un_serveur_ntp_douteux_est_refuse() {
		assert!(SettingCommand::parse(Family::Ntp, "server", "pool.ntp.org").is_some());
		assert!(SettingCommand::parse(Family::Ntp, "server", "192.168.1.10").is_some());
		for suspect in ["", " ", "ntp; rm -rf /", "http://x", "a b", "a\nb", "x".repeat(64).as_str()]
		{
			assert_eq!(
				SettingCommand::parse(Family::Ntp, "server", suspect),
				None,
				"{suspect:?} ne doit pas atteindre la caméra"
			);
		}
	}

	#[test]
	fn un_nom_de_pere_printable_et_borne() {
		assert!(SettingCommand::parse(Family::General, "device_name", "Porche").is_some());
		assert!(SettingCommand::parse(Family::General, "device_name", "Entrée ✨").is_some());
		assert_eq!(SettingCommand::parse(Family::General, "device_name", ""), None);
		assert_eq!(
			SettingCommand::parse(Family::General, "device_name", &"n".repeat(32)),
			None
		);
		assert_eq!(
			SettingCommand::parse(Family::General, "device_name", "ligne1\nligne2"),
			None,
			"un retour de ligne casserait la trame XML"
		);
	}

	#[test]
	fn le_texte_compte_en_caracteres_pas_en_octets() {
		// « Entrée » avec son é : 6 caractères, 7 octets en UTF-8. Une borne en
		// octets refuserait des noms que la caméra accepte.
		let ok = SettingCommand::parse(Family::Osd, "name", "Entrée");
		assert_eq!(ok, Some(SettingCommand::OsdName("Entrée".into())));
	}

	#[test]
	fn le_decalage_heure_d_eté_est_signe() {
		assert_eq!(
			SettingCommand::parse(Family::Dst, "offset", "-1"),
			Some(SettingCommand::DstOffset("-1".into()))
		);
		assert_eq!(SettingCommand::parse(Family::Dst, "offset", "15"), None);
		assert_eq!(SettingCommand::parse(Family::Dst, "offset", "-15"), None);
	}

	#[test]
	fn le_carillon_accepte_melodie_seule_ou_pair_complet() {
		assert_eq!(
			SettingCommand::parse(Family::Chime, "ring", "3"),
			Some(SettingCommand::ChimeRing {
				chime_id: None,
				music_id: "3".into()
			})
		);
		assert_eq!(
			SettingCommand::parse(Family::Chime, "ring", "2 7"),
			Some(SettingCommand::ChimeRing {
				chime_id: Some("2".into()),
				music_id: "7".into()
			})
		);
		assert_eq!(SettingCommand::parse(Family::Chime, "ring", "deux"), None);
		assert_eq!(SettingCommand::parse(Family::Chime, "ring", "2 sept"), None);
	}

	#[test]
	fn une_feuille_lecture_seule_n_est_pas_commandable() {
		// Le Wi-Fi, la veille, les masques et la batterie n'ont pas de SET :
		// une commande doit être refusée, pas ignorée en silence.
		for (family, leaf) in [
			(Family::Wifi, "signal"),
			(Family::Sleep, "state"),
			(Family::Mask, "enable"),
			(Family::Battery, "voltage"),
			(Family::General, "is_dst"),
		] {
			assert_eq!(
				SettingCommand::parse(family, leaf, "1"),
				None,
				"{}/{} n'est pas écrivable",
				family.slug(),
				leaf
			);
		}
	}

	#[test]
	fn une_feuille_inconnue_d_une_famille_connue_est_refusee() {
		assert_eq!(SettingCommand::parse(Family::Ntp, "mot_de_passe", "x"), None);
	}

	#[test]
	fn famille_et_feuille_de_la_commande_tombent_juste() {
		let cmd = SettingCommand::parse(Family::AutoReboot, "weekday", "Tuesday")
			.expect("mardi se commande");
		assert_eq!(cmd.family(), Family::AutoReboot);
		assert_eq!(cmd.leaf(), "weekday");
	}

	#[test]
	fn accepts_est_le_reflet_de_parse() {
		assert!(accepts("audio", "volume"));
		assert!(accepts("chime", "ring"));
		assert!(accepts("chime", "volume"));
		assert!(!accepts("wifi", "signal"));
		assert!(!accepts("general", "is_dst"));
		assert!(!accepts("inconnue", "volume"));
		assert!(!accepts("audio", "inconnu"));
		assert!(!accepts("ai", "enable"), "aucune balise d'activation n'est connue");
	}

	/// Le volume du carillon porte la plage **vue** (« 0-4 typical »), pas un
	/// 0-100 hérité du volume général. Une valeur hors de cette plage ne part
	/// pas : le 485 stockerait et répondrait 200, ce qui est un « OK » mensonger.
	#[test]
	fn volume_du_carillon_ne_sort_pas_de_la_plage_vue() {
		for v in ["0", "2", "4"] {
			let cmd = SettingCommand::parse(Family::Chime, "volume", v)
				.unwrap_or_else(|| panic!("« {v} » est dans la plage documentée"));
			assert_eq!(cmd, SettingCommand::ChimeVolume(v.to_string()));
			assert_eq!(cmd.family(), Family::Chime);
			assert_eq!(cmd.leaf(), "volume");
		}
		for v in ["5", "-1", "40", "2.5", "quatre", ""] {
			assert_eq!(
				SettingCommand::parse(Family::Chime, "volume", v),
				None,
				"« {v} » n'est pas une valeur que la documentation connaisse"
			);
		}
	}
}
