//! Les réglages de la caméra, bloc par bloc — `docs/protocole-reglages.md`
//! est le cahier des charges de ce module.
//!
//! Chaque famille suit la même loi, héritée de la lecture de la lib de
//! référence (`Api.ts` : `setAudioCfg`, `setImage`, `setMask`, `setNtp`,
//! `setDst`, `setAutoReboot`…) et rappelée au §1 du doc :
//!
//! 1. **Le SET est la réponse de la GET patchée.** Toutes les balises que la
//!    caméra a imprimées repartent ; on ne touche que celles venues changer.
//!    Le firmware « rejette (ou annule silencieusement) un bloc qui omet des
//!    champs qu'il a envoyés » — un `Ok(())` sur un bloc amputé est donc un
//!    **faux succès**, pire qu'une erreur.
//! 2. **Une balise que la caméra n'a jamais imprimée n'est pas inventée.**
//!    C'est pour ça que tous les champs sont `Option` avec
//!    `skip_serializing_if` : une caméra sans interphone n'est pas poussée à
//!    croire qu'elle en a un.
//! 3. **Les valeurs restent du texte caméra.** Un réglage qui repasse par un
//!    entier peut revenir formaté autrement (`01`, `+01`) ; le texte, lui,
//!    round-trip à l'octet près.
//! 4. **Toute écriture est suivie d'une relecture**, et c'est la relecture
//!    qui est retournée — jamais la valeur demandée. Un `SET` acquitté en 200
//!    n'est pas la preuve que la valeur est stockée.
//!
//! Pas de garde d'aptitude ici, volontairement (comme pour la famille carillon) :
//! le `abilityInfo` de ce firmware **sous-déclare** — la même famille répond
//! `405` sur `<HaCfg>` tout en acceptant `<Email>`. Une aptitude absente ne dit
//! rien de ce que la commande renvoie ; seule la caméra fait foi.

use std::sync::Arc;

use serde::Serialize;

use super::set_helpers::{SET_QUIRK_TIMEOUT, await_set_reply_with_quirk};
use super::{BcCamera, Error, Result};
use crate::bc::{model::*, xml::*};

#[cfg(test)]
use crate::bc_protocol::connection::mock::{
	reply_200_empty, reply_200_xml, reply_err_code, inspect_xml, MockConnection,
};

/// Les deux blocs que la réponse image (26) apporte ensemble, et que le
/// setter 25 redemande ensemble : écrire `bright` sans renvoyer
/// `<InputAdvanceCfg>` est l'amputation que le firmware avale en silence.
#[derive(PartialEq, Eq, Default, Debug, Clone, Serialize)]
pub struct ImageCfg {
	/// `<VideoInput>` : les cinq curseurs 0-255 et le jour/nuit.
	pub video_input: VideoInput,
	/// `<InputAdvanceCfg>` : exposition, scène, fréquence secteur. `None`
	/// quand la caméra n'a imprimé que `<VideoInput>` — auquel cas seul ce
	/// bloc repart au 25.
	pub advance: Option<InputAdvanceCfg>,
}

/// Les deux blocs que la réponse OSD (44) apporte ensemble, et que le setter
/// 45 redemande ensemble.
#[derive(PartialEq, Eq, Default, Debug, Clone, Serialize)]
pub struct OsdCfg {
	/// `<OsdDatetime>` : l'horodatage à l'écran.
	pub datetime: OsdDatetime,
	/// `<OsdChannelName>` : le nom à l'écran et son filigrane.
	pub channel_name: Option<OsdChannelName>,
}

/// Le bloc détection de mouvement, sous **la forme que la caméra a imprimée**.
///
/// Le doc (§4.1) documente une GET 46 en `<MdAlarm>` et un SET 47 en
/// `<MD version="1.1">`. Les deux existent ; une firmware donnée imprime
/// l'une ou l'autre. On ne peut patcher et renvoyer **que** ce qu'elle a
/// imprimé : `<MdAlarm>` possède des « champs libres » que personne n'a
/// énumérés, donc le reconstruire amputerait le bloc — d'où le refus de
/// [`set_motion_cfg`](Self::set_motion_cfg), qui rend l'écriture refusée au
/// lieu de la rendre faussement réussie.
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum MotionCfg {
	/// La forme du setter 47 : écriture possible.
	Md(MdCfg),
	/// La forme plate du getter 46 : lecture seule.
	MdAlarm(MdAlarm),
}

impl MotionCfg {
	/// L'état d'activation tel que la caméra l'a imprimé.
	pub fn enable(&self) -> Option<&str> {
		match self {
			MotionCfg::Md(m) => m.enable.as_deref(),
			MotionCfg::MdAlarm(m) => m.sens_info_new.as_ref().and_then(|s| s.enable.as_deref()),
		}
	}

	/// La sensibilité telle que la caméra l'a imprimée (0-50, plus haut =
	/// plus sensible, sur les firmwares qui documentent une plage).
	pub fn sensitivity(&self) -> Option<&str> {
		match self {
			MotionCfg::Md(m) => m.sensitivity_default.as_deref(),
			MotionCfg::MdAlarm(m) => m
				.sens_info_new
				.as_ref()
				.and_then(|s| s.sensitivity.as_deref()),
		}
	}

	/// Patche l'activation en place, sur le porteur lu.
	pub fn set_enable(&mut self, value: impl Into<String>) {
		let value = Some(value.into());
		match self {
			MotionCfg::Md(m) => m.enable = value,
			MotionCfg::MdAlarm(m) => {
				m.sens_info_new
					.get_or_insert_with(SensInfoNew::default)
					.enable = value;
			}
		}
	}

	/// Patche la sensibilité en place, sur le porteur lu.
	pub fn set_sensitivity(&mut self, value: impl Into<String>) {
		let value = Some(value.into());
		match self {
			MotionCfg::Md(m) => m.sensitivity_default = value,
			MotionCfg::MdAlarm(m) => {
				m.sens_info_new
					.get_or_insert_with(SensInfoNew::default)
					.sensitivity = value;
			}
		}
	}
}

impl BcCamera {
	// ── Plomberie partagée ──────────────────────────────────────────────

	/// GET d'un bloc de réglages : corps vide (le doc note « aucune requête »
	/// pour ces familles), un aller simple, et un non-200 qui devient
	/// [`Error::CameraRefused`] — « la caméra refuse cette commande » n'est
	/// jamais « le réglage est vide ». La variante, et non le jumeau sans texte,
	/// parce que ce refus-là arrive **avec** une réponse de la caméra : c'est la
	/// seule fois où le dialecte explique son non.
	async fn raw_get(&self, msg_id: u32) -> Result<Bc> {
		let connection = self.get_connection();
		let msg_num = self.new_message_num();
		let mut sub = connection.subscribe(msg_id, msg_num).await?;
		// La forme de la demande n'est plus uniforme : voir
		// [`forme_de_trame`], mesurée à la sonde MQTT le 13/11 sur cette
		// sonnette. Quatre lectures changeaient d'avis rien qu'en ajoutant
		// l'extension qui nomme le channel.
		let (canal_entete, canal_extension) = forme_of_frame(msg_id, self.channel_id);
		let req = Bc {
			meta: BcMeta {
				msg_id,
				channel_id: canal_entete,
				msg_num,
				response_code: 0,
				stream_type: 0,
				class: 0x6414,
			},
			body: BcBody::ModernMsg(match canal_extension {
				None => ModernMsg::default(),
				Some(channel) => {
					let mut ext = Extension::default();
					ext.channel_id = Some(channel);
					ModernMsg { extension: Some(ext), payload: None }
				}
			}),
		};
		sub.send(req).await?;
		let msg = sub.recv().await?;
		if msg.meta.response_code != 200 {
			// Le refus passe par ici, et le corps que la caméra a écrit **en
			// réponse au refus** repartait dans le vide : `CameraServiceUnavailable`
			// ne porte que le code. Sur cette sonnette, quatre familles meurent
			// à cette ligne et personne n'a jamais lu ce qu'elles reçoivent en
			// retour — mesuré le 13/11, après installation d'un récepteur `log`
			// qui a révélé que le pont n'en imprimait aucun.
			//
			// Une ligne par refus, au niveau `warn` : c'est le seul indice qui
			// distingue « forme de requête mauvaise » de « famille absente de ce
			// firmware » de « caméra pas wakelée ». Le texte est celui que la
			// caméra a envoyé, pas une paraphrase.
			let returned = match &msg.body {
				BcBody::ModernMsg(moderne) => {
					let extension = moderne
						.extension
						.as_ref()
						.map(text_extension)
						.unwrap_or_default();
					let body = match &moderne.payload {
						Some(BcPayloads::BcXml(bloc)) => text_xml(bloc),
						Some(BcPayloads::Binary(bytes)) => format!("<{} octets>", bytes.len()),
						None => "<pas de corps>".to_string(),
					};
					format!("{extension} {body}")
				}
				other => format!("{other:?}"),
			};
			log::warn!(
				"msg {} refusé en {} — réponse de la caméra : {}",
				msg.meta.msg_id,
				msg.meta.response_code,
				couper(&returned, 400)
			);
			// Le texte voyage avec l'erreur, et pas seulement dans le journal :
			// c'est lui que l'opératrice lira sur `status/<famille>/reason`,
			// nettoyé et borné une seconde fois plus haut par `publishable_reason`.
			return Err(Error::CameraRefused {
				id: msg.meta.msg_id,
				code: msg.meta.response_code,
				detail: couper(&returned, 200),
			});
		}
		Ok(msg)
	}

	// ── La sonde à 400 ──────────────────────────────────────────────────

	/// Demande de lecture **fabriquée à la main**, pour l'expertise des `400`.
	///
	/// [`raw_get`](Self::raw_get) n'a qu'une forme : en-tête de 24
	/// octets, ni extension ni corps. Quand une famille refuse, cette forme
	/// unique est la seule variable qu'on ait jamais variée — c'est-à-dire
	/// aucune. Cette sonde varie précisément ce qui n'a jamais dst mesuré :
	///
	/// * [`RawRequest::extension_channel`] — `<Extension><channelId>0</channelId></Extension>`,
	///   la forme qu'envoient **toutes** les commandes que cette caméra sert
	///   (audio 264, PIR 212, carillon, AbilityInfo 151) et qu'aucune des
	///   quatre familles en 400 n'envoie ;
	/// * [`RawRequest::extension_rf`] — `<rfId>`, l'autre adressage connu de
	///   ce firmware (celui du PIR) ;
	/// * [`RawRequest::corps`] — un corps qui **nomme le bloc demandé**, la
	///   forme que la doc prête à d'autres firmwares ;
	/// * [`RawRequest::canal_entete`] — l'octet 12 de l'en-tête, pour les
	///   variantes NVR.
	///
	/// Le corps passe par [`BcXml`], donc par le chiffrement normal : une
	/// variante envoyée en `BcPayloads::Binary` partirait **en clair**
	/// (`bc::ser::bc_payload_bytes` ne chiffre que les payloads XML) et
	/// confondrait « mauvaise forme de requête » et « corps non chiffré ».
	///
	/// Rien ici ne re-tente ni ne juge : la sonde rend le code et ce que la
	/// caméra a écrit, et c'est à l'opératrice de lire.
	pub async fn raw_exchange(&self, msg_id: u32, d: RawRequest) -> Result<RawResponse> {
		let connection = self.get_connection();
		let msg_num = self.new_message_num();
		let mut sub = connection.subscribe(msg_id, msg_num).await?;
		let mut ext = Extension::default();
		ext.channel_id = d.extension_channel;
		ext.rf_id = d.extension_rf;
		let meta = BcMeta {
			msg_id,
			channel_id: d.canal_entete.unwrap_or(self.channel_id),
			msg_num,
			response_code: 0,
			stream_type: 0,
			class: d.classe.unwrap_or(0x6414),
		};
		let req = match (ext.channel_id.is_some() || ext.rf_id.is_some(), d.body) {
			(true, Some(xml)) => Bc::new_from_ext_xml(meta, ext, xml),
			(true, None) => Bc::new_from_ext(meta, ext),
			(false, Some(xml)) => Bc::new_from_xml(meta, xml),
			(false, None) => Bc::new_from_meta(meta),
		};
		sub.send(req).await?;
		let msg = sub.recv().await?;
		let mut brut = RawResponse {
			code: msg.meta.response_code,
			msg_id: msg.meta.msg_id,
			classe_recue: msg.meta.class,
			kind: match &msg.body {
				BcBody::ModernMsg(_) => "modern".to_string(),
				BcBody::LegacyMsg(_) => "herite".to_string(),
			},
			..Default::default()
		};
		if let BcBody::ModernMsg(m) = &msg.body {
			brut.extension = m.extension.as_ref().map(|e| text_extension(e));
			brut.body = match &m.payload {
				Some(BcPayloads::BcXml(xml)) => Some(text_xml(xml)),
				Some(BcPayloads::Binary(bytes)) => {
					Some(format!("<{} octets binaires>", bytes.len()))
				}
				None => None,
			};
		}
		Ok(brut)
	}

	/// Sort le bloc demandé d'une réponse. Le `Bc` est rendu tel quel quand il
	/// n'y est pas, pour que l'appelant l'utilise comme ancre d'erreur sans
	/// avoir à le cloner.
	fn settings_block<T>(
		msg: Bc,
		extract: impl FnOnce(&BcXml) -> Option<T>,
	) -> std::result::Result<T, Bc> {
		match &msg.body {
			BcBody::ModernMsg(ModernMsg {
				payload: Some(BcPayloads::BcXml(xml)),
				..
			}) => match extract(xml) {
				Some(block) => Ok(block),
				None => Err(msg),
			},
			_ => Err(msg),
		}
	}

	/// SET d'un bloc de réglages, avec la quirk « certaines firmwares ne
	/// répondent pas sur les SET » : un non-200 est un refus, l'absence de
	/// réponse dans la fenêtre est un succès **à vérifier par la relecture**
	/// que l'appelant fait juste après.
	async fn raw_set(&self, msg_id: u32, xml: BcXml) -> Result<()> {
		let connection = self.get_connection();
		let msg_num = self.new_message_num();
		let mut sub = connection.subscribe(msg_id, msg_num).await?;
		// Même règle que la lecture ([`forme_de_trame`]) : le SET partait avec
		// la trame d'avant et la caméra répondait `400`, mesuré le 13/11 au
		// soir en basculant la détection de mouvement depuis Home Assistant.
		let (canal_entete, canal_extension) = forme_of_frame(msg_id, self.channel_id);
		let meta = BcMeta {
			msg_id,
			channel_id: canal_entete,
			msg_num,
			response_code: 0,
			stream_type: 0,
			class: 0x6414,
		};
		let set = match canal_extension {
			None => Bc::new_from_xml(meta, xml),
			Some(channel) => {
				let mut ext = Extension::default();
				ext.channel_id = Some(channel);
				Bc::new_from_ext_xml(meta, ext, xml)
			}
		};
		sub.send(set).await?;
		await_set_reply_with_quirk(&mut sub, SET_QUIRK_TIMEOUT).await
	}

	// ── NTP (38 / 39) ───────────────────────────────────────────────────

	/// Lit `<Ntp>` (msg 38) : activation, serveur, port et période de
	/// resynchronisation **en minutes**.
	pub async fn get_ntp(&self) -> Result<Ntp> {
		let msg = self.raw_get(MSG_ID_GET_NTP).await?;
		Self::settings_block(msg, |xml| xml.ntp.clone()).map_err(|msg| Error::UnintelligibleReply {
			reply: Arc::new(msg),
			why: "Expected Ntp xml but it was not received",
		})
	}

	/// Écrit `<Ntp>` (msg 39) puis relit le bloc (38) et renvoie **ce que la
	/// caméra a stocké après**. `cfg` doit venir de [`get_ntp`](Self::get_ntp)
	/// avec seulement les balises voulues touchées.
	pub async fn set_ntp(&self, cfg: &Ntp) -> Result<Ntp> {
		self.raw_set(
			MSG_ID_SET_NTP,
			BcXml {
				ntp: Some(cfg.clone()),
				..Default::default()
			},
		)
		.await?;
		self.get_ntp().await
	}

	// ── Redémarrage automatique (101 / 100) ─────────────────────────────

	/// Lit `<AutoReboot>` (msg 101). Attention à l'asymétrie des ids : ici le
	/// setter est le plus **petit** des deux.
	pub async fn get_auto_reboot(&self) -> Result<AutoReboot> {
		let msg = self.raw_get(MSG_ID_GET_AUTO_REBOOT).await?;
		Self::settings_block(msg, |xml| xml.auto_reboot.clone())
			.map_err(|msg| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Expected AutoReboot xml but it was not received",
			})
	}

	/// Écrit `<AutoReboot>` (msg 100) puis relit (101). Un `weekDay` hors de
	/// `Sunday`…`Saturday` / `everyday` n'est pas validé ici : c'est le texte
	/// que la caméra a imprimé qui repart, et c'est elle qui tranche.
	pub async fn set_auto_reboot(&self, cfg: &AutoReboot) -> Result<AutoReboot> {
		self.raw_set(
			MSG_ID_SET_AUTO_REBOOT,
			BcXml {
				auto_reboot: Some(cfg.clone()),
				..Default::default()
			},
		)
		.await?;
		self.get_auto_reboot().await
	}

	// ── Débruitage du micro (439 / 440) ─────────────────────────────────

	/// Lit `<aiDenoise>` (msg 439) : activation et force `1`-`100` du
	/// débruiteur appliqué au micro. Le protocole n'a **aucun** gain micro ni
	/// coupure micro ; c'est ce qui s'en rapproche le plus.
	pub async fn get_ai_denoise(&self) -> Result<AiDenoise> {
		let msg = self.raw_get(MSG_ID_GET_AI_DENOISE).await?;
		Self::settings_block(msg, |xml| xml.ai_denoise.clone())
			.map_err(|msg| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Expected aiDenoise xml but it was not received",
			})
	}

	/// Écrit `<aiDenoise>` (msg 440) puis relit (439). La casse de l'attribut
	/// de version repart telle qu'elle est arrivée — `Version` majuscule comme
	/// le documente la lib de référence, ou `version` minuscule.
	pub async fn set_ai_denoise(&self, cfg: &AiDenoise) -> Result<AiDenoise> {
		self.raw_set(
			MSG_ID_SET_AI_DENOISE,
			BcXml {
				ai_denoise: Some(cfg.clone()),
				..Default::default()
			},
		)
		.await?;
		self.get_ai_denoise().await
	}

	// ── Veille (574) et signal Wi-Fi (115) — lecture seule ──────────────

	/// Lit `<sleepState>` (msg 574) : la caméra sur batterie dort-elle ?
	///
	/// Verdict seule : aucun setter n'est documenté, et endormir un appareil
	/// sur batterie par une écriture de réglage n'est pas le rôle de ce pont.
	/// C'est aussi la seule façon de le savoir **sans le wakeler**.
	pub async fn get_sleep_state(&self) -> Result<SleepState> {
		let msg = self.raw_get(MSG_ID_GET_SLEEP_STATE).await?;
		Self::settings_block(msg, |xml| xml.sleep_state.clone())
			.map_err(|msg| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Expected sleepState xml but it was not received",
			})
	}

	/// Lit `<WifiSignal>` (msg 115). L'unité de `<signal>` n'est documentée
	/// nulle part (dBm ou %), donc le texte est relayé tel quel et aucune
	/// unité n'est revendiquée vers Home Assistant.
	///
	/// `<Wifi>` (116) n'est **pas** exposé : le firmware peut y renvoyer la
	/// clé du point d'accès, et un état MQTT est lisible par tout le broker.
	pub async fn get_wifi_signal(&self) -> Result<WifiSignal> {
		let msg = self.raw_get(MSG_ID_GET_WIFI_SIGNAL).await?;
		Self::settings_block(msg, |xml| xml.wifi_signal.clone())
			.map_err(|msg| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Expected WifiSignal xml but it was not received",
			})
	}

	// ── Image (26 / 25) ─────────────────────────────────────────────────

	/// Lit les blocs image (msg 26) : `<VideoInput>` et, le plus souvent dans
	/// la même réponse, `<InputAdvanceCfg>`.
	///
	/// Une réponse 200 qui n'apporte **aucun** des deux est
	/// [`Error::UnintelligibleReply`] : « la caméra n'a rien dit » ne doit
	/// jamais se lire comme « les curseurs sont à zéro ».
	pub async fn get_image_cfg(&self) -> Result<ImageCfg> {
		let msg = self.raw_get(MSG_ID_GET_IMAGE).await?;
		let (video_input, advance) = match &msg.body {
			BcBody::ModernMsg(ModernMsg {
				payload: Some(BcPayloads::BcXml(xml)),
				..
			}) => (xml.video_input.clone(), xml.input_advance_cfg.clone()),
			_ => (None, None),
		};
		Ok(ImageCfg {
			video_input: video_input.ok_or_else(|| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Expected VideoInput xml but it was not received",
			})?,
			advance,
		})
	}

	/// Écrit les blocs image (msg 25) — **les deux**, puis relit (26) et
	/// renvoie la relecture.
	pub async fn set_image_cfg(&self, cfg: &ImageCfg) -> Result<ImageCfg> {
		self.raw_set(
			MSG_ID_SET_IMAGE,
			BcXml {
				video_input: Some(cfg.video_input.clone()),
				input_advance_cfg: cfg.advance.clone(),
				..Default::default()
			},
		)
		.await?;
		self.get_image_cfg().await
	}

	// ── OSD (44 / 45) ───────────────────────────────────────────────────

	/// Lit les blocs OSD (msg 44) : `<OsdDatetime>` et `<OsdChannelName>`.
	///
	/// L'OSD n'existe **que** là : le bug historique de la lib de référence
	/// (lire l'OSD sur le 26, écrire `<Osd>` sur le 25) produisait des écritures
	/// jamais appliquées — il n'est pas rejoué ici.
	pub async fn get_osd_cfg(&self) -> Result<OsdCfg> {
		let msg = self.raw_get(MSG_ID_GET_OSD).await?;
		let (datetime, channel_name) = match &msg.body {
			BcBody::ModernMsg(ModernMsg {
				payload: Some(BcPayloads::BcXml(xml)),
				..
			}) => (xml.osd_datetime.clone(), xml.osd_channel_name.clone()),
			_ => (None, None),
		};
		Ok(OsdCfg {
			datetime: datetime.ok_or_else(|| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Expected OsdDatetime xml but it was not received",
			})?,
			channel_name,
		})
	}

	/// Écrit les blocs OSD (msg 45), les deux ensemble, puis relit (44).
	/// `<topLeftX>`/`<topLeftY>` sont des 16.16 normalisés : ils repartent
	/// comme ils sont arrivés, jamais recalculés.
	pub async fn set_osd_cfg(&self, cfg: &OsdCfg) -> Result<OsdCfg> {
		self.raw_set(
			MSG_ID_SET_OSD,
			BcXml {
				osd_datetime: Some(cfg.datetime.clone()),
				osd_channel_name: cfg.channel_name.clone(),
				..Default::default()
			},
		)
		.await?;
		self.get_osd_cfg().await
	}

	// ── Détection de mouvement (46 / 47) ────────────────────────────────

	/// Lit la détection de mouvement (msg 46), sous la forme que la caméra a
	/// imprimée : [`MotionCfg::Md`] si elle a répondu `<MD>`,
	/// [`MotionCfg::MdAlarm`] si elle a répondu `<MdAlarm>`.
	pub async fn get_motion_cfg(&self) -> Result<MotionCfg> {
		let msg = self.raw_get(MSG_ID_GET_MOTION_DETECT).await?;
		let (md, md_alarm) = match &msg.body {
			BcBody::ModernMsg(ModernMsg {
				payload: Some(BcPayloads::BcXml(xml)),
				..
			}) => (xml.md.clone(), xml.md_alarm.clone()),
			_ => (None, None),
		};
		if let Some(m) = md {
			return Ok(MotionCfg::Md(m));
		}
		if let Some(m) = md_alarm {
			return Ok(MotionCfg::MdAlarm(m));
		}
		Err(Error::UnintelligibleReply {
			reply: Arc::new(msg),
			why: "Expected MD or MdAlarm xml but neither was received",
		})
	}

	/// Écrit la détection de mouvement (msg 47) puis relit (46).
	///
	/// N'écrit que le porteur `<MD>` : c'est la forme que le setter documente.
	/// Sur un porteur `<MdAlarm>`, renvoie [`Error::Usage`] **sans envoyer une
	/// octave** — reconstruire ce bloc dropperait ses « champs libres », et le
	/// firmware encaisserait ça en silence.
	pub async fn set_motion_cfg(&self, cfg: &MotionCfg) -> Result<MotionCfg> {
		match cfg {
			MotionCfg::Md(m) => {
				self.raw_set(
					MSG_ID_SET_MOTION_DETECT,
					BcXml {
						md: Some(m.clone()),
						..Default::default()
					},
				)
				.await?;
				self.get_motion_cfg().await
			}
			MotionCfg::MdAlarm(_) => Err(Error::Usage(
				"refusing to write motion detection: the camera answered 46 with <MdAlarm>, \
				 whose unenumerated fields a rewritten block would drop; only <MD> is writable"
					.to_string(),
			)),
		}
	}

	// ── Masques de confidentialité (52) — lecture seule ─────────────────

	/// Lit `<Shelter>` (msg 52) : l'activation du masquage et le nombre
	/// d'emplacements que le modèle offre.
	///
	/// **Pas de setter 53** : les balises enfants de `shelterList` (et le
	/// codage `pixel << 16 | canvas` des coordonnées) n'ont aucune capture
	/// locale, et renvoyer un `<Shelter>` amputé de ses masques est précisément
	/// l'écriture que le firmware avale sans rien dire. Le refus est donc dans
	/// la conception, pas dans l'appel.
	pub async fn get_privacy_masks(&self) -> Result<Shelter> {
		let msg = self.raw_get(MSG_ID_GET_PRIVACY_MASK).await?;
		Self::settings_block(msg, |xml| xml.shelter.clone())
			.map_err(|msg| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Expected Shelter xml but it was not received",
			})
	}

	// ── Heure d'dst (106 / 107) ─────────────────────────────────────────

	/// Écrit `<Dst>` (msg 107) puis relit (106) et renvoie la relecture.
	///
	/// `cfg` doit venir de [`get_dst`](Self::get_dst) : le calendrier
	/// (`startMonth`, `startWeekIndex`, `startWeekday`, …) repart intact, seule
	/// la balise touchée change.
	pub async fn set_dst(&self, cfg: &Dst) -> Result<Dst> {
		self.raw_set(
			MSG_ID_SET_DST,
			BcXml {
				dst: Some(cfg.clone()),
				..Default::default()
			},
		)
		.await?;
		self.get_dst().await
	}

	// ── Système (104 / 105) ─────────────────────────────────────────────

	/// Lit `<SystemGeneral>` (msg 104) : fuseau, formats d'affichage, langue,
	/// nom système de l'appareil.
	///
	/// Le nom système n'est **pas** le nom à l'écran
	/// (`<OsdChannelName><name>`, msg 45) ni le nom MQTT de la caméra : ce sont
	/// trois choses distinctes.
	pub async fn get_general(&self) -> Result<SystemGeneral> {
		let msg = self.raw_get(MSG_ID_GET_GENERAL).await?;
		Self::settings_block(msg, |xml| xml.system_general.clone())
			.map_err(|msg| Error::UnintelligibleReply {
				reply: Arc::new(msg),
				why: "Reply did not contain a SystemGeneral block",
			})
	}

	/// Écrit `<SystemGeneral>` (msg 105) **sans toucher l'horloge**, puis relit
	/// (104) et renvoie la relecture.
	///
	/// Deux marqueurs imposés par le firmware (`docs/protocole-reglages.md`
	/// §6.2, payloads confirmés par pcap) sont posés ici et pas ailleurs :
	///
	/// * `<year>0</year>` = « ne pas recaler l'heure manuelle ». Sans lui la
	///   caméra recale l'horloge sur les champs `year…second` du bloc, ce
	///   qu'un changement de langue ou de nom n'a pas à faire.
	/// * `<deviceNameOnly>1</deviceNameOnly>` = « seul le nom change », requis
	///   quand le patch ne touche que `deviceName`. Il n'est **pas** posé quand
	///   le patch touche aussi autre chose, parce que ce serait mentir au
	///   firmware sur le contenu du payload.
	pub async fn set_general(&self, cfg: &SystemGeneral) -> Result<SystemGeneral> {
		let mut out = cfg.clone();
		out.year = Some(0);
		let only_name = out.device_name.is_some()
			&& out.time_zone.is_none()
			&& out.osd_format.is_none()
			&& out.time_format.is_none()
			&& out.language.is_none();
		if only_name {
			out.device_name_only = Some(1);
		}
		self.raw_set(
			MSG_ID_SET_GENERAL,
			BcXml {
				system_general: Some(out),
				..Default::default()
			},
		)
		.await?;
		self.get_general().await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Un bloc `<aiDenoise>` tel que la lib de référence l'imprime : attribut
	/// de version **majuscule**, `level` entre 1 et 100.
	fn denoise_block() -> AiDenoise {
		AiDenoise {
			version_caps: Some("1.1".into()),
			version: None,
			enable: Some("1".into()),
			level: Some("80".into()),
			channel_id: None,
		}
	}

	fn ntp_block() -> Ntp {
		Ntp {
			version: "1.1".into(),
			channel_id: Some("0".into()),
			enable: Some("1".into()),
			server: Some("pool.ntp.org".into()),
			port: Some("123".into()),
			synchronize_interval: Some("1440".into()),
			time_zone: Some("-3600".into()),
		}
	}

	fn ntp_reply(req: &Bc) -> Bc {
		reply_200_xml(
			req,
			BcXml {
				ntp: Some(ntp_block()),
				..Default::default()
			},
		)
	}

	// ── NTP ─────────────────────────────────────────────────────────────

	#[tokio::test]
	async fn get_ntp_relaye_the_text_camera() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_NTP)
			.reply_with(ntp_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let ntp = cam.get_ntp().await.expect("ok");
		assert_eq!(ntp.server.as_deref(), Some("pool.ntp.org"));
		assert_eq!(ntp.enable.as_deref(), Some("1"));
		assert_eq!(ntp.synchronize_interval.as_deref(), Some("1440"));
		assert_eq!(ntp.time_zone.as_deref(), Some("-3600"));
	}

	#[tokio::test]
	async fn get_ntp_distingue_refusal_and_empty() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_NTP)
			.reply_with(|req| reply_err_code(req, 400))
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let err = cam.get_ntp().await.expect_err("400");
		assert!(
			matches!(
				err,
				Error::CameraRefused { code: 400, .. }
					| Error::CameraServiceUnavailable { code: 400, .. }
			),
			"un refus doit rester un refus, pas un bloc vide : {err:?}"
		);
		// Et depuis le 13/11, une lecture de réglage refusée porte le texte que
		// la caméra a écrit sous son code — c'est la seule trace qui dise
		// « mauvaise forme » ou « famille absente ».
		if let Error::CameraRefused { detail, .. } = &err {
			assert!(!detail.is_empty(), "le refus de lecture doit porter la réponse caméra");
		}
	}

	#[tokio::test]
	async fn get_ntp_200_without_block_is_inintelligible() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_NTP)
			.reply_with(reply_200_empty)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		assert!(matches!(
			cam.get_ntp().await.expect_err("no block"),
			Error::UnintelligibleReply { .. }
		));
	}

	#[tokio::test]
	async fn set_ntp_retourne_the_reread_and_not_the_request() {
		// Caméra têtue : elle acquitte le 39 mais garde son serveur. La valeur
		// retournée doit être celle de la relecture.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_NTP)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let ntp = sent.ntp.as_ref().expect("le SET porte <Ntp>");
				assert_eq!(
					ntp.server.as_deref(),
					Some("time.nist.gov"),
					"la balise patchée doit repartir"
				);
				assert_eq!(
					ntp.synchronize_interval.as_deref(),
					Some("1440"),
					"une balise non touchée doit repartir telle quelle"
				);
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_NTP)
			.reply_with(ntp_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut want = ntp_block();
		want.server = Some("time.nist.gov".into());
		let after = cam.set_ntp(&want).await.expect("ok");
		assert_eq!(
			after.server.as_deref(),
			Some("pool.ntp.org"),
			"la relecture fait foi, jamais la demande"
		);
	}

	#[tokio::test]
	async fn set_ntp_rejete_not_rereads_not() {
		// Le script ne prévoit délibérément aucun 38 : une relecture après un
		// refus échouerait le mock — c'est l'assertion voulue (une octave de
		// radio économisée sur un appareil sur batterie).
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_NTP)
			.reply_with(|req| reply_err_code(req, 409))
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let err = cam.set_ntp(&ntp_block()).await.expect_err("409");
		assert!(
			matches!(err, Error::CameraServiceUnavailable { code: 409, .. }),
			"{err:?}"
		);
	}

	// ── Débruitage micro ────────────────────────────────────────────────

	#[tokio::test]
	async fn get_ai_denoise_accepts_the_version_majuscule() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_AI_DENOISE)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						ai_denoise: Some(denoise_block()),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let d = cam.get_ai_denoise().await.expect("ok");
		assert_eq!(d.version_caps.as_deref(), Some("1.1"));
		assert_eq!(d.level.as_deref(), Some("80"));
	}

	#[tokio::test]
	async fn set_ai_denoise_renvoie_the_casse_read_without_inventer_the_other() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_AI_DENOISE)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let d = sent.ai_denoise.as_ref().expect("<aiDenoise>");
				assert_eq!(d.version_caps.as_deref(), Some("1.1"));
				assert!(
					d.version.is_none(),
					"le casier de version non utilisé ne doit pas être inventé"
				);
				assert_eq!(d.level.as_deref(), Some("80"));
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_AI_DENOISE)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						ai_denoise: Some(denoise_block()),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut want = denoise_block();
		want.enable = Some("0".into());
		let after = cam.set_ai_denoise(&want).await.expect("ok");
		// La caméra du script n'a pas bougé : c'est bien sa relecture.
		assert_eq!(after.enable.as_deref(), Some("1"));
	}

	// ── Image : les deux blocs ensemble ─────────────────────────────────

	fn video_input_block() -> VideoInput {
		VideoInput {
			version: "1.1".into(),
			channel_id: Some("0".into()),
			bright: Some("128".into()),
			contrast: Some("130".into()),
			saturation: Some("120".into()),
			hue: Some("128".into()),
			sharpen: Some("60".into()),
			ir_cut_swap: Some("0".into()),
			day_night: Some("auto".into()),
			corridor_ability: Some("0".into()),
			corridor_mode: None,
		}
	}

	fn advance_block() -> InputAdvanceCfg {
		InputAdvanceCfg {
			version: "1.1".into(),
			channel_id: Some("0".into()),
			exposure: Some(ExposureCfg {
				mode: Some("auto".into()),
			}),
			scene: Some("normal".into()),
			power_line_frequency: Some("50".into()),
			digital_channel: None,
		}
	}

	fn image_reply(req: &Bc) -> Bc {
		reply_200_xml(
			req,
			BcXml {
				video_input: Some(video_input_block()),
				input_advance_cfg: Some(advance_block()),
				..Default::default()
			},
		)
	}

	#[tokio::test]
	async fn get_image_cfg_apporte_the_deux_blocks() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(image_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let cfg = cam.get_image_cfg().await.expect("ok");
		assert_eq!(cfg.video_input.bright.as_deref(), Some("128"));
		assert_eq!(cfg.video_input.day_night.as_deref(), Some("auto"));
		assert_eq!(
			cfg.advance
				.as_ref()
				.and_then(|a| a.exposure.as_ref())
				.and_then(|e| e.mode.as_deref()),
			Some("auto")
		);
	}

	#[tokio::test]
	async fn set_image_cfg_renvoie_the_deux_blocks_au_25() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_IMAGE)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				assert!(
					sent.video_input.is_some() && sent.input_advance_cfg.is_some(),
					"le 25 doit recevoir VideoInput ET InputAdvanceCfg"
				);
				let v = sent.video_input.as_ref().expect("<VideoInput>");
				assert_eq!(v.sharpen.as_deref(), Some("90"), "le curseur patché repart");
				assert_eq!(v.bright.as_deref(), Some("128"), "les autres repartent");
				assert!(
					v.corridor_mode.is_none(),
					"une balise absente de la GET n'est pas inventée"
				);
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(image_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut cfg = ImageCfg {
			video_input: video_input_block(),
			advance: Some(advance_block()),
		};
		cfg.video_input.sharpen = Some("90".into());
		let after = cam.set_image_cfg(&cfg).await.expect("ok");
		assert_eq!(
			after.video_input.sharpen.as_deref(),
			Some("60"),
			"la relecture fait foi"
		);
	}

	#[tokio::test]
	async fn get_image_cfg_200_without_block_is_inintelligible() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(reply_200_empty)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		assert!(matches!(
			cam.get_image_cfg().await.expect_err("no block"),
			Error::UnintelligibleReply { .. }
		));
	}

	// ── OSD ─────────────────────────────────────────────────────────────

	fn osd_cfg() -> OsdCfg {
		OsdCfg {
			datetime: OsdDatetime {
				version: "1.1".into(),
				channel_id: Some("0".into()),
				enable: Some("1".into()),
				top_left_x: Some("1".into()),
				top_left_y: Some("1".into()),
				width: Some("602".into()),
				height: Some("91".into()),
				language: Some("English".into()),
			},
			channel_name: Some(OsdChannelName {
				version: "1.1".into(),
				channel_id: Some("0".into()),
				name: Some("Porche".into()),
				enable: Some("0".into()),
				top_left_x: Some("1".into()),
				top_left_y: Some("65535".into()),
				en_watermark: Some("0".into()),
				en_bgcolor: Some("0".into()),
			}),
		}
	}

	fn osd_reply(req: &Bc) -> Bc {
		let cfg = osd_cfg();
		reply_200_xml(
			req,
			BcXml {
				osd_datetime: Some(cfg.datetime),
				osd_channel_name: cfg.channel_name,
				..Default::default()
			},
		)
	}

	#[tokio::test]
	async fn get_osd_cfg_apporte_the_deux_blocks() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_OSD)
			.reply_with(osd_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let cfg = cam.get_osd_cfg().await.expect("ok");
		assert_eq!(cfg.datetime.enable.as_deref(), Some("1"));
		assert_eq!(
			cfg.channel_name.as_ref().and_then(|c| c.name.as_deref()),
			Some("Porche")
		);
	}

	#[tokio::test]
	async fn set_osd_cfg_renvoie_the_deux_blocks_without_recalculer_the_coordonnees() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_OSD)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let dn = sent
					.osd_datetime
					.as_ref()
					.expect("le 45 recoit <OsdDatetime>");
				let cn = sent
					.osd_channel_name
					.as_ref()
					.expect("le 45 recoit <OsdChannelName>");
				assert_eq!(dn.enable.as_deref(), Some("0"), "la balise patchée");
				assert_eq!(dn.top_left_x.as_deref(), Some("1"));
				assert_eq!(dn.top_left_y.as_deref(), Some("1"));
				assert_eq!(cn.top_left_y.as_deref(), Some("65535"), "les 16.16 repartent");
				assert_eq!(cn.name.as_deref(), Some("Porche"), "le nom repart");
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_OSD)
			.reply_with(osd_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut cfg = osd_cfg();
		cfg.datetime.enable = Some("0".into());
		let after = cam.set_osd_cfg(&cfg).await.expect("ok");
		assert_eq!(after.datetime.enable.as_deref(), Some("1"));
	}

	// ── Détection de mouvement : les deux porteurs ──────────────────────

	fn md_block() -> MdCfg {
		MdCfg {
			version: "1.1".into(),
			channel_id: Some("0".into()),
			enable: Some("1".into()),
			usepir: Some("0".into()),
			width: Some("60".into()),
			height: Some("33".into()),
			scope: Some(MdScope {
				columns: Some("96".into()),
				rows: Some("64".into()),
				value_table: Some("AAAA".into()),
			}),
			sensitivity_default: Some("30".into()),
		}
	}

	fn md_reply(req: &Bc) -> Bc {
		reply_200_xml(
			req,
			BcXml {
				md: Some(md_block()),
				..Default::default()
			},
		)
	}

	#[tokio::test]
	async fn motion_on_md_writes_without_abimer_the_zones() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(md_reply)
			.expect_msg(MSG_ID_SET_MOTION_DETECT)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let m = sent.md.as_ref().expect("le 47 recoit <MD>");
				assert_eq!(m.enable.as_deref(), Some("0"));
				assert_eq!(
					m.scope.as_ref().and_then(|s| s.value_table.as_deref()),
					Some("AAAA"),
					"le bitmap de zones ne doit pas être abîmé"
				);
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(md_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut cfg = cam.get_motion_cfg().await.expect("ok");
		assert_eq!(cfg.enable(), Some("1"));
		assert_eq!(cfg.sensitivity(), Some("30"));
		cfg.set_enable("0");
		let after = cam.set_motion_cfg(&cfg).await.expect("ok");
		assert_eq!(after.enable(), Some("1"), "la relecture fait foi");
	}

	#[test]
	fn the_forme_of_frame_is_bornee_aux_quatre_families_mesurees() {
		// Ces quatre familles refusaient sans rien dire ; la mesure du 13/11 dit
		// qu'il leur faut le channel, dans l'extension et dans l'en-tête — en
		// lecture, et en écriture par symétrie (le `47` refuse sans lui).
		for id in [
			MSG_ID_GET_IMAGE,
			MSG_ID_SET_IMAGE,
			MSG_ID_GET_OSD,
			MSG_ID_SET_OSD,
			MSG_ID_GET_MOTION_DETECT,
			MSG_ID_SET_MOTION_DETECT,
			MSG_ID_GET_PRIVACY_MASK,
			MSG_ID_SET_PRIVACY_MASK,
		] {
			assert_eq!(
				forme_of_frame(id, 7),
				(0, Some(0)),
				"le msg {id} doit partir avec le channel 0 partout"
			);
		}
		// Et ce qui répond déjà ne bouge pas : aucune mesure ne dit que ces
		// familles-là accepteraient la nouvelle trame.
		for id in [
			MSG_ID_GET_NTP,
			MSG_ID_SET_NTP,
			MSG_ID_GET_GENERAL,
			MSG_ID_BATTERY_INFO,
		] {
			assert_eq!(
				forme_of_frame(id, 7),
				(7, None),
				"le msg {id} doit garder sa trame d'aujourd'hui"
			);
		}
	}

	#[tokio::test]
	async fn ecrire_the_mouvement_port_the_channel_comme_the_read() {
		// Home Assistant basculait l'interrupteur sans effet : le `47` partait
		// sans l'extension et la caméra répondait `400`. Le SET doit donc partir
		// avec la même forme que le GET — et c'est la relecture, déjà dans
		// `set_motion_cfg`, qui dira si l'écriture a porté.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(md_reply)
			.expect_msg(MSG_ID_SET_MOTION_DETECT)
			.reply_with(|req| {
				let BcBody::ModernMsg(msg) = &req.body else {
					panic!("la sonnette est en trame moderne");
				};
				assert_eq!(
					msg.extension.as_ref().and_then(|e| e.channel_id),
					Some(0),
					"le 47 sans extension est refusé 400 : l'interrupteur ne bascule pas"
				);
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(md_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut cfg = cam.get_motion_cfg().await.expect("lu");
		cfg.set_enable("0");
		cam.set_motion_cfg(&cfg).await.expect("écrit");
	}

	#[tokio::test]
	async fn motion_46_part_the_channel_qui_lui_fait_respondre() {
		// Le 46 répond 400 **sans corps** tant que la demande ne nomme pas le
		// channel. Ce test porte la forme sur la vraie chaîne d'envoi, pas
		// seulement sur la table : une table juste mal appliquée ne se verrait
		// pas, la caméra ne disant jamais pourquoi elle refuse.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(|req| {
				let BcBody::ModernMsg(msg) = &req.body else {
					panic!("la sonnette est en trame moderne");
				};
				assert_eq!(
					msg.extension.as_ref().and_then(|e| e.channel_id),
					Some(0),
					"sans le channel dans l'extension, la caméra répond 400 sans corps"
				);
				assert_eq!(req.meta.channel_id, 0, "l'en-tête doit porter le même channel");
				md_reply(req)
			})
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let cfg = cam.get_motion_cfg().await.expect("lu");
		assert_eq!(cfg.enable(), Some("1"));
	}

	#[tokio::test]
	async fn motion_on_md_alarm_refuse_without_nothing_send() {
		// Le script ne prévoit QUE le 46 : l'envoi d'un 47 ferait échouer le
		// mock, ce qui est exactement l'assertion voulue.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						md_alarm: Some(MdAlarm {
							version: "1.1".into(),
							channel_id: Some("0".into()),
							sens_info_new: Some(SensInfoNew {
								enable: Some("1".into()),
								sensitivity: Some("3".into()),
								alarm_type: Some("MdAlarm".into()),
							}),
						}),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut cfg = cam.get_motion_cfg().await.expect("ok");
		assert!(matches!(cfg, MotionCfg::MdAlarm(_)));
		assert_eq!(cfg.sensitivity(), Some("3"));
		cfg.set_enable("0");
		let err = cam.set_motion_cfg(&cfg).await.expect_err("refus");
		match err {
			Error::Usage(msg) => {
				assert!(msg.contains("MdAlarm"), "le refus nomme la balise lue : {msg}")
			}
			other => panic!("attendu Usage, obtenu {other:?}"),
		}
	}

	#[tokio::test]
	async fn motion_200_without_porteur_is_inintelligible() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(reply_200_empty)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		assert!(matches!(
			cam.get_motion_cfg().await.expect_err("no block"),
			Error::UnintelligibleReply { .. }
		));
	}

	// ── Veille, Wi-Fi, masques ──────────────────────────────────────────

	#[tokio::test]
	async fn sleep_state_and_wifi_signal_itself_lisent() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_SLEEP_STATE)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						sleep_state: Some(SleepState {
							version: "1.1".into(),
							sleep: Some("1".into()),
							mode: Some("2".into()),
							pan_pos: None,
							tilt_pos: None,
							image_name: Some("sleep.jpg".into()),
						}),
						..Default::default()
					},
				)
			})
			.expect_msg(MSG_ID_GET_WIFI_SIGNAL)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						wifi_signal: Some(WifiSignal {
							version: "1.1".into(),
							channel_id: Some("0".into()),
							signal: Some("-67".into()),
						}),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		assert_eq!(
			cam.get_sleep_state().await.expect("s").sleep.as_deref(),
			Some("1")
		);
		assert_eq!(
			cam.get_wifi_signal().await.expect("w").signal.as_deref(),
			Some("-67")
		);
	}

	#[tokio::test]
	async fn masques_itself_lisent() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_PRIVACY_MASK)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						shelter: Some(Shelter {
							version: "1.1".into(),
							channel_id: Some("0".into()),
							enable: Some("0".into()),
							max_num: Some("4".into()),
							track_enable: None,
						}),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let s = cam.get_privacy_masks().await.expect("ok");
		assert_eq!(s.max_num.as_deref(), Some("4"));
		assert_eq!(s.enable.as_deref(), Some("0"));
	}

	// ── Redémarrage automatique ─────────────────────────────────────────

	fn reboot_block() -> AutoReboot {
		AutoReboot {
			version: "1.1".into(),
			enable: Some("1".into()),
			week_day: Some("everyday".into()),
			hour: Some("3".into()),
			minute: Some("30".into()),
			second: Some("0".into()),
		}
	}

	fn reboot_reply(req: &Bc) -> Bc {
		reply_200_xml(
			req,
			BcXml {
				auto_reboot: Some(reboot_block()),
				..Default::default()
			},
		)
	}

	#[tokio::test]
	async fn auto_reboot_writes_on_the_right_id_and_rereads() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_AUTO_REBOOT)
			.reply_with(reboot_reply)
			.expect_msg(MSG_ID_SET_AUTO_REBOOT)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let a = sent
					.auto_reboot
					.as_ref()
					.expect("le 100 recoit <AutoReboot>");
				assert_eq!(a.enable.as_deref(), Some("0"));
				assert_eq!(a.hour.as_deref(), Some("3"), "l'heure non touchée repart");
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_AUTO_REBOOT)
			.reply_with(reboot_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut cfg = cam.get_auto_reboot().await.expect("ok");
		cfg.enable = Some("0".into());
		let after = cam.set_auto_reboot(&cfg).await.expect("ok");
		assert_eq!(after.enable.as_deref(), Some("1"));
		assert_eq!(after.week_day.as_deref(), Some("everyday"));
	}

	// ── Heure d'dst ─────────────────────────────────────────────────────

	#[tokio::test]
	async fn set_dst_rereads_the_106_after_the_107() {
		let dst = Dst {
			version: "1.1".into(),
			enable: Some(1),
			offset: Some(1),
			start_month: Some(3),
			..Default::default()
		};
		let again = dst.clone();
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_DST)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let d = sent.dst.as_ref().expect("le 107 recoit <Dst>");
				assert_eq!(d.enable, Some(0), "la bascule patchée repart");
				assert_eq!(d.start_month, Some(3), "le calendrier repart");
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_DST)
			.reply_with(move |req| {
				reply_200_xml(
					req,
					BcXml {
						dst: Some(again.clone()),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let mut want = dst.clone();
		want.enable = Some(0);
		let after = cam.set_dst(&want).await.expect("ok");
		assert_eq!(after.enable, Some(1), "la caméra a gardé son activation");
	}

	// ── Système ─────────────────────────────────────────────────────────

	fn general_block() -> SystemGeneral {
		SystemGeneral {
			version: "1.1".into(),
			time_zone: Some(-3600),
			year: Some(2026),
			month: Some(11),
			day: Some(11),
			hour: Some(18),
			minute: Some(35),
			second: Some(4),
			osd_format: Some("DMY".into()),
			time_format: Some(0),
			language: Some("English".into()),
			device_name: Some("Porche".into()),
			..Default::default()
		}
	}

	fn general_reply(req: &Bc) -> Bc {
		reply_200_xml(
			req,
			BcXml {
				system_general: Some(general_block()),
				..Default::default()
			},
		)
	}

	#[tokio::test]
	async fn set_general_not_touches_not_horloge_and_marks_the_renommage_seul() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_GENERAL)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let g = sent
					.system_general
					.as_ref()
					.expect("le 105 recoit <SystemGeneral>");
				assert_eq!(g.year, Some(0), "<year>0</year> est obligatoire");
				assert_eq!(
					g.device_name_only,
					Some(1),
					"un renommage seul porte <deviceNameOnly>1</deviceNameOnly>"
				);
				assert_eq!(g.device_name.as_deref(), Some("Entrée"));
				assert_eq!(g.language.as_deref(), None, "rien d'autre n'est ajouté");
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_GENERAL)
			.reply_with(general_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let want = SystemGeneral {
			version: "1.1".into(),
			device_name: Some("Entrée".into()),
			..Default::default()
		};
		let after = cam.set_general(&want).await.expect("ok");
		assert_eq!(after.device_name.as_deref(), Some("Porche"), "relecture");
		assert_eq!(after.time_zone, Some(-3600));
	}

	#[tokio::test]
	async fn set_general_not_marks_not_renommage_seul_quand_il_y_a_other_thing() {		let mock = MockConnection::new()
			.expect_msg(MSG_ID_SET_GENERAL)
			.reply_with(|req| {
				let sent = inspect_xml(req);
				let g = sent.system_general.as_ref().expect("<SystemGeneral>");
				assert_eq!(
					g.device_name_only, None,
					"marquer « nom seul » alors que la langue change aussi ment au firmware"
				);
				assert_eq!(g.year, Some(0));
				assert_eq!(g.language.as_deref(), Some("French"));
				reply_200_empty(req)
			})
			.expect_msg(MSG_ID_GET_GENERAL)
			.reply_with(general_reply)
			.build()
			.await;
		let cam = BcCamera::from_mock_connection(mock).await;
		let want = SystemGeneral {
			version: "1.1".into(),
			device_name: Some("Entrée".into()),
			language: Some("French".into()),
			..Default::default()
		};
		cam.set_general(&want).await.expect("ok");
	}

	/// Ce que le wire contient vraiment. Les tests ci-dessus relisent le XML
	/// désérialisé ; celui-ci regarde l'**octet stream**, parce que c'est lui
	/// que le firmware jugera : casse des balises (`<MD>` pas `<Md>`,
	/// `<aiDenoise Version>` majuscule), et surtout l'absence totale de balise
	/// qu'une caméra n'a pas imprimée.
	#[test]
	fn the_wire_invente_no_tag() {
		let xml = BcXml {
			ntp: Some(ntp_block()),
			ai_denoise: Some(denoise_block()),
			md: Some(md_block()),
			..Default::default()
		};
		let wire = quick_xml::se::to_string(&xml).expect("infaillible");
		assert!(wire.starts_with("<body"), "corps enveloppé : {wire}");
		assert!(wire.contains(r#"<Ntp version="1.1">"#), "casse de <Ntp> : {wire}");
		assert!(
			wire.contains(r#"<aiDenoise Version="1.1">"#),
			"l'attribut de version majuscule doit repartir tel quel : {wire}"
		);
		assert!(
			wire.contains(r#"<MD version="1.1">"#),
			"<MD> garde sa casse : {wire}"
		);
		assert!(wire.contains("<sensitivityDefault>30</sensitivityDefault>"));
		assert!(
			!wire.contains("MdAlarm"),
			"le porteur non lu n'est pas sérialisé : {wire}"
		);
		for absente in ["<Shelter", "<sleepState", "<WifiSignal", "<VideoInput"] {
			assert!(
				!wire.contains(absente),
				"{absente} n'a pas dst lu donc ne doit pas être écrit : {wire}"
			);
		}
	}
}

/// Ce que la caméra a répondu à une [`RawRequest`] : le code, et les deux
/// moitiés du corps **telles qu'elles ont dst comprises**.
#[derive(Debug, Default, Clone)]
pub struct RawResponse {
	/// Le `response_code` de l'en-tête : 200, 400, 405…
	pub code: u16,
	/// Le `msg_id` rendu par la caméra (le nôtre, d'habitude).
	pub msg_id: u32,
	/// La `class` de l'en-tête reçu. Utile autant que le code : une caméra qui
	/// répondrait dans une autre classe que celle demandée parle un autre
	/// dialecte, et ça ne se voit nulle part ailleurs.
	pub classe_recue: u16,
	/// De quelle varidst était le corps reçu : `modern`, `herite`, ou `rien`.
	///
	/// Sans ce champ, une réponse **héritée** se lirait comme un corps vide —
	/// le décodage ci-dessous ne regarde que `ModernMsg`. La mesure du 13/11
	/// (« pas de corps ») n'a de sens que parce que cette varidst-là était bien
	/// `modern` : en-tête moderne, aucun bloc derrière.
	pub kind: String,
	/// L'`<Extension>` décryptée, en XML.
	pub extension: Option<String>,
	/// Le corps décrypté, rendu en XML.
	///
	/// **Ce texte n'est pas la capture brute** : il est resérialisé depuis le
	/// `BcXml` parsé, donc les balises que ce dépôt ne modélise pas y sont
	/// absentes. Un 400 dont le corps ne dirait que `<result>notSupport</result>`
	/// se lira ici comme un corps vide — le texte intégral de la caméra ne sort
	/// que de la trace `Payload Txt`, et c'est justement ce que cette sonde est
	/// là pour rendre inutile de payer deux fois.
	pub body: Option<String>,
}

/// Une variante de la demande de lecture. Tout à `None` = exactement la trame
/// d'aujourd'hui, ce qui sert de témoin dans la série. Consommée par l'appel :
/// `BcXml` n'est pas `Clone`, et le corps part tel quel sur le câble.
#[derive(Debug, Default)]
pub struct RawRequest {
	/// `channelId` **d'en-tête** (octet 12). `None` = celui de la caméra.
	pub canal_entete: Option<u8>,
	/// `Some(n)` : ajoute `<channelId>n</channelId>` à l'extension.
	pub extension_channel: Option<u8>,
	/// `Some(n)` : ajoute `<rfId>n</rfId>` à l'extension.
	pub extension_rf: Option<u8>,
	/// `Some(xml)` : le corps à envoyer, typé donc chiffré.
	pub body: Option<BcXml>,
	/// `class` de l'en-tête. `None` = `0x6414`, la classe XML moderne.
	///
	/// La variable qui restait à varier après la mesure du 13/11 : les quatre
	/// familles en 400 répondent **sans aucun corps**, alors que des lectures
	/// envoyées à l'identique — `ntp` 38, `denoise` 439, `sleep` 574 — rendent
	/// 200. La forme du corps n'est donc pas la seule différence possible entre
	/// une commande servie et une commande refusée ; la classe déclarée de la
	/// requête en est une autre, et elle ne coûtait rien à exposer.
	pub classe: Option<u16>,
}

impl RawRequest {
	/// Le témoin : la trame que la bibliothèque envoie aujourd'hui.
	pub fn temoin() -> Self {
		Self::default()
	}

	/// Extension avec `<channelId>`, la forme des commandes qui passent.
	pub fn channel(n: u8) -> Self {
		Self { extension_channel: Some(n), ..Default::default() }
	}

	/// Extension avec `<rfId>`, l'adressage du PIR.
	pub fn rf(n: u8) -> Self {
		Self { extension_rf: Some(n), ..Default::default() }
	}

	/// Un corps qui nomme le bloc, sans extension.
	pub fn body(xml: BcXml) -> Self {
		Self { body: Some(xml), ..Default::default() }
	}

	/// La forme complète attendue par les clients de référence : extension
	/// **et** corps.
	pub fn channel_and_body(n: u8, xml: BcXml) -> Self {
		Self { extension_channel: Some(n), body: Some(xml), ..Default::default() }
	}
}

/// La forme que **ce** micrologiciel attend pour une trame de réglage — lecture
/// **et** écriture —, mesurée le 13/11/2026 avec la sonde MQTT
/// ([`super::settings_brut`], pilotée depuis
/// `bairelay/<cam>/control/sonde/<serial>`).
///
/// Quatre familles refusaient `400` sans jamais écrire de réponse. La variable
/// n'était ni l'éveil, ni le corps : c'est l'absence d'`<Extension>` nommant le
/// channel. Séance du 13/11, caméra éveillée, session unique tenue par le pont :
///
/// | demande | réponse |
/// |---|---|
/// | `msg 46` seul | `400`, aucun corps |
/// | `msg 46` + `<channelId>0</channelId>` | `200` — `<MD version="1.1">…` |
/// | `msg 46` + un corps `<MD>`, sans extension | `400` |
/// | `msg 26` + `<channelId>0</channelId>` | `200` — `<Shelter>…` |
/// | `msg 44` + extension seule | `400` |
/// | `msg 44` + extension **et** `channelId` d'en‑tête à `0` | `200` — `<OsdDatetime>…` |
/// | `msg 52` + extension seule | `400` |
/// | `msg 52` + extension et en‑tête à `0` | `200` — `<OsdDatetime>…` |
/// | `msg 38` (ntp) et `104` (general) dans la même forme | `200` — la forme ne casse pas ce qui marchait |
/// | `msg 44` avec `<channelId>1</channelId>` | `421` — le micrologiciel lit le channel et le refuse |
/// | `msg 47` (SET mouvement) **sans** extension, depuis Home Assistant | `400` — l'interrupteur reste à `on` |
///
/// La règle appliquée est donc **restreinte à ces quatre familles**, en lecture
/// comme en écriture, et non rendue générale : `ntp`, `general`, `battery` et
/// les autres répondent déjà avec la trame d'avant, et aucune mesure ne dit
/// qu'ils accepteraient celle‑ci. Une règle générale non mesurée sur les
/// familles qui marchent aujourd'hui serait troquer quatre pannes connues
/// contre dix pannes éventuelles.
///
/// Côté écriture, la table porte les identifiants `SET` par symétrie avec la
/// lecture : c'est une **inférence**, pas une mesure — ce qu'on a mesuré, c'est
/// que le `47` refuse `400` sans l'extension. L'inférence reste contrôlable sur
/// le terrain parce que chaque `set_*` relit le bloc après avoir écrit : une
/// écriture avalée sans effet y laisse une trace, et c'est la relecture qui fait
/// foi jusque dans les tests.
///
/// Deux choses que cette table ne dit **pas** : les créneaux de ce micrologiciel
/// ne suivent pas la table usuelle — le `26` répond `<Shelter>` (les masques) et
/// le `52` répond `<OsdDatetime>`, là où la bibliothèque attendait
/// `<VideoInput>` et `<Shelter>` ; et rien ici ne dit où ce micrologiciel sert
/// les réglages d'image.
fn forme_of_frame(msg_id: u32, canal_camera: u8) -> (u8, Option<u8>) {
	match msg_id {
		MSG_ID_GET_IMAGE
		| MSG_ID_SET_IMAGE
		| MSG_ID_GET_OSD
		| MSG_ID_SET_OSD
		| MSG_ID_GET_MOTION_DETECT
		| MSG_ID_SET_MOTION_DETECT
		| MSG_ID_GET_PRIVACY_MASK
		| MSG_ID_SET_PRIVACY_MASK => (0, Some(0)),
		_ => (canal_camera, None),
	}
}

/// Rend le XML d'un bloc reçu. `pub(crate)` : la sonde est la seule à en avoir
/// besoin, et le texte de la caméra n'a rien à faire dans les API de plus haut
/// niveau (mêmes précautions que `publishable_reason` côté MQTT).
fn text_xml(xml: &BcXml) -> String {
	let mut tampon = Vec::new();
	match xml.serialize(&mut tampon) {
		Ok(_) => String::from_utf8_lossy(&tampon).into_owned(),
		Err(e) => format!("<bloc reçu mais non resérialisable : {e}>"),
	}
}

fn text_extension(ext: &Extension) -> String {
	let mut tampon = Vec::new();
	match ext.serialize(&mut tampon) {
		Ok(_) => String::from_utf8_lossy(&tampon).into_owned(),
		Err(e) => format!("<extension non resérialisable : {e}>"),
	}
}

/// Couper un texte rendu **par caractères**, jamais par octets : une réponse de
/// caméra peut porter de l'UTF-8 et un `&t[..400]` sur un octet au milieu d'un
/// accent panique. Le journal est fait pour être lu, pas pour être exhaustif.
fn couper(texte: &str, caracteres: usize) -> String {
	if texte.chars().count() <= caracteres {
		return texte.to_string();
	}
	let mut coupe: String = texte.chars().take(caracteres).collect();
	coupe.push_str(" […]");
	coupe
}
