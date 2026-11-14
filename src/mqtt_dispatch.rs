//! Routes incoming MQTT control commands to the appropriate BcCamera methods.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use bairelay_mqtt::control::{ControlCommand, IrMode, PtzDirection};
use bairelay_mqtt::settings::Family;
use bairelay_mqtt::{topics, SharedMqttClient, StatusPublisher};

use bairelay_neolink_core::bc_protocol::{CameraDriver, Direction, Error, LightState};

use crate::camera::CameraHandle;
use crate::camera_tasks::{publish_after_led_write, LedWrite};
use crate::status_cache::StatusCache;

pub async fn dispatch_control(
	cmd: ControlCommand,
	cameras: &HashMap<String, Arc<CameraHandle>>,
	mqtt: &SharedMqttClient,
	topic_prefix: &str,
) {
	let camera_name = cmd.camera_name().to_owned();
	let Some(cam) = cameras.get(&camera_name) else {
		tracing::warn!(camera = %camera_name, "Control for unknown camera");
		return;
	};

	// Wakeup is special — it must work even when camera is disconnected,
	// because its purpose is to trigger the connection via wake lock.
	if let ControlCommand::Wakeup { minutes, .. } = &cmd {
		tracing::info!(camera = %camera_name, minutes, "Wakeup requested");
		let wl = cam.wake_lock().clone();
		let cam_clone = Arc::clone(cam);
		let cancel = cam_clone.cancel_token().clone();
		let mins = *minutes;
		tokio::spawn(async move {
			let _guard = wl.acquire();
			// Wait for camera to connect, with timeout and cancellation
			let connect_timeout = tokio::time::timeout(Duration::from_secs(120), async {
				loop {
					if cam_clone.state().is_connected() {
						return;
					}
					tokio::time::sleep(Duration::from_millis(500)).await;
				}
			});
			tokio::select! {
				_ = cancel.cancelled() => return,
				result = connect_timeout => {
					if result.is_err() {
						tracing::warn!(camera = %cam_clone.name(), "Wakeup timed out waiting for camera to connect");
						return;
					}
				}
			}
			// Camera connected — now start the countdown
			tokio::select! {
				_ = cancel.cancelled() => {},
				_ = tokio::time::sleep(Duration::from_secs(mins as u64 * 60)) => {},
			}
		});
		return;
	}

	// Acquire the wake lock FIRST. If the camera is sleeping, this is
	// what triggers the connection — the camera task is parked on
	// `wait_for_acquire()`. Holding the guard until dispatch ends keeps
	// the camera awake for the duration of the command.
	let _guard = cam.wake_lock().acquire();
	let reply_topic = cmd.control_topic(topic_prefix);
	tracing::debug!(camera = %camera_name, command = %reply_topic, "Dispatching control command");

	let bc = match cam.bc_camera() {
		Some(bc) => bc,
		None => {
			// Camera is asleep / connecting. The wake-lock acquire above
			// has already kicked off the connect; wait for it (bounded)
			// and respect the camera's cancellation token. Argus
			// cameras come up in 1-3 s in practice; 15 s gives margin
			// for slow discovery without making a stuck command sit
			// here for a minute.
			tracing::info!(
				camera = %camera_name,
				"Camera not connected; waking and waiting before dispatch"
			);
			match wait_for_bc_camera(cam, Duration::from_secs(15)).await {
				Some(bc) => bc,
				None => {
					tracing::warn!(
						camera = %camera_name,
						"Timed out waiting for camera to connect; dropping command"
					);
					return;
				}
			}
		}
	};

	const CMD_TIMEOUT: Duration = Duration::from_secs(30);
	// Une relecture de réglages est une douzaine de commandes en série (et le
	// carillon en est cinq à lui seul) ; le plafond d'une écriture simple ne lui
	// suffit pas. Borné quand même : une synchronisation qui pendouille n'est pas
	// une synchronisation qui attendra le prochain wake.
	const SETTINGS_TIMEOUT: Duration = Duration::from_secs(120);

	let result = match cmd {
		ControlCommand::Floodlight { state, .. } => {
			timeout_to_result(timeout(CMD_TIMEOUT, bc.set_floodlight_manual(state, 30)).await)
		}
		ControlCommand::FloodlightTasks { state, .. } => {
			timeout_to_result(timeout(CMD_TIMEOUT, bc.floodlight_tasks_enable(state)).await)
		}
		ControlCommand::Led {
			camera: ref cam_name,
			state,
		} => {
			let _led = cam.led_write_lock().lock().await;
			let result = timeout_to_result(timeout(CMD_TIMEOUT, bc.led_light_set(state)).await);
			// The command always goes out; state is published only for
			// confirmed hardware — otherwise the re-read may be a placeholder.
			let present = cam
				.capabilities()
				.is_some_and(|c| c.status_led.is_present());
			if result.is_ok() && present {
				let publisher = StatusPublisher::new(mqtt, topic_prefix, cam_name);
				let cache = cam.status_cache();
				let written = LedWrite::Led(state);
				publish_after_led_write(&*bc, &publisher, &cache, written).await;
			}
			result
		}
		ControlCommand::Ir {
			camera: ref cam_name,
			mode,
		} => {
			let ls = match mode {
				IrMode::On => LightState::On,
				IrMode::Off => LightState::Off,
				IrMode::Auto => LightState::Auto,
			};
			let _led = cam.led_write_lock().lock().await;
			let result = timeout_to_result(timeout(CMD_TIMEOUT, bc.irled_light_set(ls)).await);
			let present = cam.capabilities().is_some_and(|c| c.ir_leds.is_present());
			if result.is_ok() && present {
				let publisher = StatusPublisher::new(mqtt, topic_prefix, cam_name);
				let cache = cam.status_cache();
				let written = LedWrite::Ir(mode);
				publish_after_led_write(&*bc, &publisher, &cache, written).await;
			}
			result
		}
		ControlCommand::Pir {
			camera: ref cam_name,
			state,
		} => {
			let result = timeout_to_result(timeout(CMD_TIMEOUT, bc.pir_set(state)).await);
			if result.is_ok() {
				// La commande est partie ; l'état, lui, vient de la **relecture**
				// du matériel. Publier `state` publierait l'intention : un
				// firmware qui hoche la tête sans rien changer laisserait chez
				// Home Assistant un état que plus rien ne contredit — et comme le
				// topic est conservé, il survivrait en plus au redémarrage du pont.
				publish_pir_after_write(&*bc, mqtt, topic_prefix, cam_name, &cam.status_cache(), state)
					.await;
			}
			result
		}
		ControlCommand::Reboot { .. } => timeout_to_result(timeout(CMD_TIMEOUT, bc.reboot()).await),
		ControlCommand::Ptz {
			direction, amount, ..
		} => {
			let dir = match direction {
				PtzDirection::Up => Direction::Up,
				PtzDirection::Down => Direction::Down,
				PtzDirection::Left => Direction::Left,
				PtzDirection::Right => Direction::Right,
			};
			let speed = 32.0_f32;
			let seconds = (amount / speed).clamp(0.0, 10.0);
			match timeout(CMD_TIMEOUT, bc.send_ptz(dir, speed)).await {
				Ok(Err(e)) => {
					tracing::warn!(camera = %camera_name, error = %e, "PTZ move failed");
				}
				Err(_) => {
					tracing::warn!(camera = %camera_name, "PTZ move timed out");
				}
				Ok(Ok(())) => {
					tokio::time::sleep(Duration::from_secs_f32(seconds)).await;
				}
			}
			// Always try to stop
			let _ = timeout(CMD_TIMEOUT, bc.send_ptz(Direction::Stop, speed)).await;
			Ok(())
		}
		ControlCommand::PtzPreset {
			ref camera,
			preset_id,
		} => {
			let result =
				timeout_to_result(timeout(CMD_TIMEOUT, bc.moveto_ptz_preset(preset_id)).await);
			if result.is_ok() {
				if let Some(name) = cam.preset_name_for_id(preset_id) {
					let topic = topics::status_ptz_preset(topic_prefix, camera);
					let _ = mqtt.publish_retained(&topic, name.as_bytes()).await;
				}
			}
			result
		}
		ControlCommand::PtzPresetByName {
			ref camera,
			ref name,
		} => match cam.preset_id_for_name(name) {
			Some(preset_id) => {
				let result =
					timeout_to_result(timeout(CMD_TIMEOUT, bc.moveto_ptz_preset(preset_id)).await);
				if result.is_ok() {
					let topic = topics::status_ptz_preset(topic_prefix, camera);
					let _ = mqtt.publish_retained(&topic, name.as_bytes()).await;
				}
				result
			}
			None => {
				// Cache-miss must surface as FAIL to the operator —
				// previously this returned Ok(()) and the dispatch loop
				// published `OK` on the reply topic, so an HA automation
				// or `mosquitto_pub` user saw "success" while the camera
				// never moved. The cache is populated on connect and on
				// `query/ptz/preset`; a miss means either a typo or a
				// stale dashboard.
				tracing::warn!(camera = %camera, name = %name, "PTZ preset name not in cache");
				Err(bairelay_neolink_core::bc_protocol::Error::Other(
					"PTZ preset name not in cache",
				))
			}
		},
		ControlCommand::PtzAssign {
			preset_id, name, ..
		} => timeout_to_result(timeout(CMD_TIMEOUT, bc.set_ptz_preset(preset_id, name)).await),
		ControlCommand::Zoom { level, .. } => {
			timeout_to_result(timeout(CMD_TIMEOUT, bc.zoom_to((level * 1000.0) as u32)).await)
		}
		ControlCommand::Siren { state, .. } => {
			if state {
				timeout_to_result(timeout(CMD_TIMEOUT, bc.siren()).await)
			} else {
				Ok(())
			}
		}
		// ── Réglages (`control/<famille>/<feuille>`) ────────────────────
		// La valeur a déjà passé les bornes du protocole dans
		// `bairelay_mqtt::settings`. Ce qui reste est le geste de la maison :
		// relire le bloc, n'y retoucher que la balise demandée, l'écrire, relire,
		// et publier la relecture — jamais la valeur demandée.
		ControlCommand::Setting { setting, .. } => {
			let family = setting.family();
			match cam.bc_camera_concrete() {
				Some(bc_cfg) => {
					let publisher = StatusPublisher::new(mqtt, topic_prefix, &camera_name);
					let brut = timeout(
						CMD_TIMEOUT,
						crate::settings::apply_setting(&bc_cfg, &publisher, &setting),
					)
					.await;
					// Le délai est distingué de la réponse de la caméra : le
					// motif publié doit dire lequel des deux s'est produit, et
					// `timeout_to_result` fond les deux en une seule erreur.
					let en_delay = brut.is_err();
					let resultat = timeout_to_result(brut);
					match &resultat {
						Ok(()) => {
							// Une écriture qui a réussi a aussi **relu** le bloc :
							// la famille est donc connue et réécrivable. C'est ce
							// moment — et pas un sondage périodique — qui fait
							// passer ses entités de « lisibles » à « modifiables ».
							cam.mark_family_read(family, true).await;
							// Et la famille va bien : la raison d'un refus
							// précédent n'a plus rien à faire sur le broker.
							crate::settings::clear_reason(&publisher, family).await;
						}
						Err(e) => {
							// Le `FAIL` sur le topic de commande est éphémère :
							// personne ne le retrouve dix minutes plus tard dans
							// Home Assistant. Le motif, lui, est conservé.
							let phrase = if en_delay {
								crate::settings::REASON_TIMEOUT.to_string()
							} else {
								crate::settings::cause(e)
							};
							crate::settings::publish_reason(&publisher, family, &phrase).await;
							// Les états déjà lus ne sont pas effacés : la caméra
							// n'a pas dit autre chose. En revanche une famille
							// jamais relue ne devient pas modifiable pour autant.
						}
					}
					resultat
				}
				// Les réglages ne sont pas sur le trait `CameraDriver` : sans le
				// `BcCamera` concret (session en cours de fermeture, ou faux
				// pilote), rien n'est écrit, et le FAIL le dit.
				None => {
					tracing::warn!(
						camera = %camera_name,
						leaf = setting.leaf(),
						"réglage sans session Baïchuan concrète : rien n'a dst envoyé"
					);
					// Rien n'a dst envoyé, et ce n'est pas la caméra qui a dit
					// non : c'est le pont. Le FAIL est éphémère, la phrase reste.
					let publisher = StatusPublisher::new(mqtt, topic_prefix, &camera_name);
					crate::settings::publish_reason(
						&publisher,
						family,
						crate::settings::REASON_SANS_SESSION,
					)
					.await;
					Err(Error::Other("settings need a connected Baichuan session"))
				}
			}
		}
		ControlCommand::RejectedSetting { family, leaf, .. } => {
			// La caméra n'a **rien** reçu : valeur hors plage, texte trop long,
			// ou feuille en lecture seule. Un FAIL sur le topic d'origine est la
			// seule réponse honnête — une commande refusée qui se tait est
			// indistinguable d'un pont mort.
			tracing::warn!(
				camera = %camera_name,
				family = family.slug(),
				leaf = %leaf,
				"valeur de réglage refusée avant d'atteindre la caméra"
			);
			// Un réglage refusé ne doit plus être « juste silencieux » : le FAIL
			// part sur le topic de commande, et la raison reste lisible sur
			// `status/<famille>/reason`. La caméra n'a rien reçu, donc rien n'est
			// effacé et la famille ne change pas de plateforme — elle garde ce
			// qu'une lecture antérieure a prouvé.
			let publisher = StatusPublisher::new(mqtt, topic_prefix, &camera_name);
			crate::settings::publish_reason(
				&publisher,
				family,
				"valeur refusée avant d'atteindre la caméra : hors des bornes du protocole",
			)
			.await;
			Err(Error::Other(
				"setting value rejected before it reached the camera",
			))
		}
		ControlCommand::Wakeup { .. } => {
			// Handled above (before connection check)
			unreachable!()
		}
		// Query commands -- serialize response as XML to status topic
		// (non-retained), matching neolink behavior.
		ControlCommand::QueryBattery { ref camera, .. } => {
			match timeout(CMD_TIMEOUT, bc.battery_info()).await {
				Ok(Ok(info)) => {
					let topic = topics::status_battery(topic_prefix, camera);
					if let Ok(xml) = serialize_xml(&info) {
						let _ = mqtt.publish(&topic, xml.as_bytes()).await;
					}
				}
				Ok(Err(e)) => tracing::warn!(camera = %camera, error = %e, "Battery query failed"),
				Err(_) => tracing::warn!(camera = %camera, "Battery query timed out"),
			}
			Ok(())
		}
		ControlCommand::QuerySettings { family, .. } => {
			// Relecture à la demande explicite. Chaque famille est une commande
			// caméra — sur une sonnette sur batterie, `query/settings` sans
			// famille en costaud une quinzaine : c'est le prix d'un appui
			// d'opératrice ou d'une automatisation volontaire, et c'est bien pour
			// ça que rien ici n'est périodique.
			match cam.bc_camera_concrete() {
				None => {
					tracing::warn!(
						camera = %camera_name,
						"synchronisation des réglages sans session Baïchuan concrète"
					);
					Err(Error::Other("settings need a connected Baichuan session"))
				}
				Some(bc_cfg) => {
					let publisher = StatusPublisher::new(mqtt, topic_prefix, &camera_name);
					// Une entrée par famille relue, avec son verdict : modifiable,
					// verrouillée par une preuve locale, ou undeterminede (la
					// discovery ne bouge alors pas). Le `Vec` se remplit au fil
					// de l'eau : une synchronisation complète interrompue par le
					// délai conserve quand même ce qui a dst lu avant l'arrêt.
					let mut read: Vec<(Family, crate::settings::Verdict)> = Vec::new();
					match family {
						Some(f) => read.push(sync_one(&bc_cfg, &publisher, f).await),
						None => {
							// Le `fut` emprunte `lues` : il doit être relâché
							// avant de lire le vecteur, d'où le `let`.
							let interrompue = timeout(
								SETTINGS_TIMEOUT,
								crate::settings::sync_all(&bc_cfg, &publisher, &mut read),
							)
							.await
							.is_err();
							if interrompue {
								tracing::warn!(
									camera = %camera_name,
									read = read.len(),
									"synchronisation des réglages interrompue"
								);
							}
						}
					}
					// La discovery suit la lecture : c'est ici que les familles
					// relues passent des capteurs de bonne foi aux entités
					// modifiables, en une seule rafale (payload vide sur l'ancien
					// topic de discovery avant la nouvelle configuration).
					cam.mark_families_read(&read).await;
					tracing::info!(
						camera = %camera_name,
						read = read.len(),
						writable = read
							.iter()
							.filter(|(_, v)| matches!(v, crate::settings::Verdict::Writable))
							.count(),
						// Verdicts promis **malgré** un refus de lecture : la
						// famille est modifiable et son topic `reason` porte la
						// raison. Les compter à part est ce qui fait voir, dans un
						// journal, une salve maussade sans avoir à relire MQTT.
						passageres = read
							.iter()
							.filter(|(_, v)| matches!(v, crate::settings::Verdict::Transient))
							.count(),
						indetermines = read
							.iter()
							.filter(|(_, v)| matches!(v, crate::settings::Verdict::Unknown))
							.count(),
						"réglages relus et publiés"
					);
					// `OK` veut dire « la relecture a eu lieu », pas « chaque
					// famille a répondu » : ce que la caméra a refusé se lit sur
					// les états effacés **et** sur les topics `reason`.
					Ok(())
				}
			}
		}
		ControlCommand::QueryPreview { ref camera } => {
			match timeout(CMD_TIMEOUT, bc.get_snapshot()).await {
				Ok(Ok(jpeg)) => {
					// One-shot render (no shared cache); cheaper than
					// threading the per-camera OverlayCache in for a
					// single operator-triggered query.
					let payload = crate::preview_overlay::rendered_preview(
						bytes::Bytes::from(jpeg),
						cam.config().pause.preview_overlay,
						*cam.preview_state_rx().borrow(),
						None,
					);
					let publisher = StatusPublisher::new(mqtt, topic_prefix, camera);
					if let Err(e) = publisher.publish_preview(&payload).await {
						tracing::warn!(camera = %camera, error = %e, "QueryPreview publish failed");
					}
				}
				Ok(Err(e)) => {
					tracing::warn!(camera = %camera, error = %e, "QueryPreview snapshot failed")
				}
				Err(_) => tracing::warn!(camera = %camera, "QueryPreview snapshot timed out"),
			}
			Ok(())
		}
		ControlCommand::QueryPir { ref camera, .. } => {
			match timeout(CMD_TIMEOUT, bc.get_pirstate()).await {
				Ok(Ok(pir_state)) => {
					let topic = topics::status_pir(topic_prefix, camera);
					if let Ok(xml) = serialize_xml(&pir_state) {
						let _ = mqtt.publish(&topic, xml.as_bytes()).await;
					}
				}
				Ok(Err(e)) => tracing::warn!(camera = %camera, error = %e, "PIR query failed"),
				Err(_) => tracing::warn!(camera = %camera, "PIR query timed out"),
			}
			Ok(())
		}
		ControlCommand::QueryPtzPreset { ref camera } => {
			match timeout(CMD_TIMEOUT, bc.get_ptz_preset()).await {
				Ok(Ok(p)) => {
					let presets: Vec<(u8, String)> = p
						.preset_list
						.preset
						.into_iter()
						.filter_map(|preset| preset.name.map(|n| (preset.id, n)))
						.collect();
					cam.replace_preset_cache(presets);
					if let Err(e) = cam.publish_discovery().await {
						tracing::warn!(
							camera = %camera,
							error = %e,
							"QueryPtzPreset: discovery republish failed"
						);
					}
				}
				Ok(Err(e)) => {
					tracing::warn!(camera = %camera, error = %e, "QueryPtzPreset: get_ptz_preset failed")
				}
				Err(_) => {
					tracing::warn!(camera = %camera, "QueryPtzPreset: get_ptz_preset timed out")
				}
			}
			Ok(())
		}
	};

	// Publish OK/FAIL reply (non-retained) to the control topic.
	let reply = if result.is_ok() { "OK" } else { "FAIL" };
	tracing::debug!(camera = %camera_name, command = %reply_topic, result = reply, "Command completed");
	let _ = mqtt.publish(&reply_topic, reply.as_bytes()).await;

	if let Err(e) = result {
		tracing::warn!(camera = %camera_name, command = %reply_topic, error = %e, "Control command failed");
	}
}

/// Wait until the camera's `bc_camera()` returns `Some`, polling every
/// 250 ms up to `deadline`. Cancellation token wins immediately.
/// Returns `None` on timeout or cancellation.
async fn wait_for_bc_camera(
	cam: &Arc<CameraHandle>,
	deadline: Duration,
) -> Option<Arc<dyn bairelay_neolink_core::bc_protocol::CameraDriver>> {
	let cancel = cam.cancel_token().clone();
	let cam_clone = Arc::clone(cam);
	let inner = async move {
		loop {
			if let Some(bc) = cam_clone.bc_camera() {
				return Some(bc);
			}
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
	};
	tokio::select! {
		_ = cancel.cancelled() => None,
		result = tokio::time::timeout(deadline, inner) => result.ok().flatten(),
	}
}

/// La relecture qui suit une écriture est bornée court : la caméra est wakelée
/// (le wake lock est tenu), et une relecture qui traîne ne doit pas rendre
/// l'opératrice impatiente — elle a déjà son OK/FAIL.
const REREAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Après une écriture du PIR, relit le matériel et publie **ce qu'il a gardé**.
///
/// Une relecture qui échoue ne publie rien : le dernier état conservé reste en
/// place plutôt que d'être remplacé par la valeur demandée. C'est la règle de
/// toute écriture MQTT de ce pont — un état décrit la caméra, jamais l'intention
/// qu'on a eue à son sujet. Avant cette relecture, l'état PIR n'était que le
/// écho de la commande : un firmware qui hoche la tête sans rien changer ne
/// pouvait pas être vu, et comme le topic est conservé, le mensonge survivait au
/// redémarrage du pont.
async fn publish_pir_after_write(
	bc: &dyn CameraDriver,
	mqtt: &SharedMqttClient,
	topic_prefix: &str,
	cam_name: &str,
	cache: &Arc<StatusCache>,
	demanded: bool,
) {
	let reread = match timeout(REREAD_TIMEOUT, bc.get_pirstate()).await {
		Ok(Ok(cfg)) => cfg,
		Ok(Err(e)) => {
			tracing::warn!(
				camera = %cam_name,
				error = %e,
				"relecture du PIR après écriture impossible : aucun état publié"
			);
			return;
		}
		Err(_) => {
			tracing::warn!(
				camera = %cam_name,
				"relecture du PIR après écriture expirée : aucun état publié"
			);
			return;
		}
	};
	let kept = reread.enable != 0;
	let publisher = StatusPublisher::new(mqtt, topic_prefix, cam_name);
	if let Err(e) = publisher.publish_pir(kept).await {
		tracing::warn!(camera = %cam_name, error = %e, "publication de l'état PIR relu a échoué");
	}
	// La sensibilité a sa propre feuille ; `reglages::publish_pir` porte la règle
	// d'orthographe (`sensiValue` sur les firmwares récents, `sensitivity` avant).
	crate::settings::publish_pir(&publisher, &reread).await;
	if kept != demanded {
		tracing::warn!(
			camera = %cam_name,
			demanded,
			kept,
			"la caméra a accepté l'écriture PIR puis gardé une autre valeur : c'est elle qui est publiée"
		);
	}
	cache.set_pir(kept);
}

/// Relit une seule famille et rend le verdict que la discovery doit publier.
///
/// Échec = états effacés **et** motif publié ; `reglages::read_family` s'en
/// charge pour ce que la caméra a refusé, et le délai, qui se déclenche **ici**,
/// est mis en mot à la main. Un délai ne prouve pas que le réglage est interdit —
/// il prouve que la caméra n'a pas répondu — donc il rend le même verdict qu'un
/// refus passager, [`crate::settings::Verdict::Transient`] : la famille reste
/// modifiable et la raison s'affiche à côté. Ce qui ne doit plus se promettre,
/// c'est l'inverse : une famille dont on n'a **jamais** rien lu, et cette règle-là
/// tient parce qu'une famille jamais relue reste absente de la discovery
/// modifiable jusqu'à son premier verdict.
async fn sync_one(
	bc: &bairelay_neolink_core::bc_protocol::BcCamera,
	publ: &StatusPublisher<'_>,
	family: Family,
) -> (Family, crate::settings::Verdict) {
	match timeout(REREAD_TIMEOUT * 3, crate::settings::read_family(bc, publ, family)).await {
		Ok(read) => read,
		Err(_) => {
			tracing::warn!(
				family = family.slug(),
				"relecture d'une famille de réglages expirée : états effacés, motif publié, famille laissée modifiable"
			);
			crate::settings::clear_family(publ, family).await;
			crate::settings::publish_reason_transient(publ, family, crate::settings::REASON_TIMEOUT)
				.await;
			(family, crate::settings::Verdict::Transient)
		}
	}
}

/// Collapse a `timeout(..).await` outcome into a plain camera-result,
/// converting an `Err(Elapsed)` into our canonical "Command timed out"
/// error. Pulled out so the 11 identical call-sites in this module
/// share one implementation — and so the failure-path test doesn't
/// need a fake camera that hangs 30 s.
pub(crate) fn timeout_to_result<T>(
	outcome: std::result::Result<
		std::result::Result<T, bairelay_neolink_core::bc_protocol::Error>,
		tokio::time::error::Elapsed,
	>,
) -> std::result::Result<T, bairelay_neolink_core::bc_protocol::Error> {
	outcome.unwrap_or_else(|_| {
		Err(bairelay_neolink_core::bc_protocol::Error::Other(
			"Command timed out",
		))
	})
}

/// Serialize a serde-compatible struct to XML string (matches neolink's
/// quick-xml serialization for query responses).
fn serialize_xml<T: serde::Serialize>(value: &T) -> Result<String, String> {
	let mut buf = bytes::BytesMut::new();
	quick_xml::se::to_writer(&mut buf, value)
		.map_err(|e| format!("XML serialization failed: {e}"))?;
	String::from_utf8(buf.to_vec()).map_err(|e| format!("XML encoding failed: {e}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::camera::CameraHandle;
	use crate::config::test_helpers::minimal_camera_config;
	use bairelay_neolink_core::bc_protocol::{CameraDriver, FakeCamera, FakeCameraBuilder};
	use std::sync::Arc as StdArc;
	use tokio_util::sync::CancellationToken;

	#[test]
	fn timeout_to_result_forwards_ok_value() {
		let r: std::result::Result<u32, bairelay_neolink_core::bc_protocol::Error> =
			timeout_to_result(Ok(Ok(42)));
		assert_eq!(r.unwrap(), 42);
	}

	#[test]
	fn timeout_to_result_forwards_inner_error() {
		let r: std::result::Result<u32, bairelay_neolink_core::bc_protocol::Error> =
			timeout_to_result(Ok(Err(
				bairelay_neolink_core::bc_protocol::Error::AuthFailed,
			)));
		assert!(matches!(
			r.unwrap_err(),
			bairelay_neolink_core::bc_protocol::Error::AuthFailed
		));
	}

	#[tokio::test]
	async fn timeout_to_result_elapsed_becomes_other_command_timed_out() {
		let elapsed = tokio::time::timeout(
			Duration::from_millis(1),
			std::future::pending::<
				std::result::Result<(), bairelay_neolink_core::bc_protocol::Error>,
			>(),
		)
		.await
		.unwrap_err();
		let r: std::result::Result<(), bairelay_neolink_core::bc_protocol::Error> =
			timeout_to_result(Err(elapsed));
		let err = r.unwrap_err();
		assert!(format!("{err}").contains("Command timed out"));
	}

	fn test_cameras(name: &str) -> HashMap<String, Arc<CameraHandle>> {
		let cancel = CancellationToken::new();
		let handle = Arc::new(CameraHandle::new(minimal_camera_config(name), cancel, None));
		let mut map = HashMap::new();
		map.insert(name.to_string(), handle);
		map
	}

	/// Build a [`CameraHandle`] with a pre-built [`FakeCamera`] installed
	/// via [`CameraHandle::set_driver_for_test`], returning a cameras
	/// map ready for [`dispatch_control`] plus the retained `FakeCamera`
	/// handle so the test can assert on [`FakeCamera::calls`] after the
	/// dispatcher runs.
	fn test_cameras_with_fake(
		name: &str,
		fake: StdArc<FakeCamera>,
	) -> (HashMap<String, Arc<CameraHandle>>, StdArc<FakeCamera>) {
		let cancel = CancellationToken::new();
		let handle = Arc::new(CameraHandle::new(minimal_camera_config(name), cancel, None));
		let driver: Arc<dyn CameraDriver> = fake.clone();
		handle.set_driver_for_test(driver);
		let mut map = HashMap::new();
		map.insert(name.to_string(), handle);
		(map, fake)
	}

	#[tokio::test]
	async fn dispatch_unknown_camera_returns_early() {
		let cameras = test_cameras("cam-known");
		let mqtt = SharedMqttClient::for_test_stub("dispatch-unknown");
		let cmd = ControlCommand::Floodlight {
			camera: "cam-missing".to_string(),
			state: true,
		};
		// Must not panic; must return quickly (early-exit).
		dispatch_control(cmd, &cameras, &mqtt, "bairelay").await;
	}

	#[tokio::test]
	async fn dispatch_disconnected_camera_waits_then_drops_on_cancel() {
		// Camera is Disconnected and there is no run loop, so the
		// wake-lock acquire never lands a `bc_camera`. Dispatch should
		// wait, then bail when the camera's cancel token fires —
		// without panicking and without spinning forever. The bound is
		// 60 s (production timeout); cancellation breaks out earlier.
		let cameras = test_cameras("cam-disc");
		let mqtt = SharedMqttClient::for_test_stub("dispatch-disc");
		let cmd = ControlCommand::Led {
			camera: "cam-disc".to_string(),
			state: false,
		};
		// Cancel before dispatch so wait_for_bc_camera returns immediately.
		cameras.get("cam-disc").unwrap().cancel_token().cancel();
		dispatch_control(cmd, &cameras, &mqtt, "bairelay").await;
	}

	#[tokio::test]
	async fn dispatch_wakeup_spawns_task_without_panicking() {
		// Wakeup is the only command that tolerates a disconnected
		// camera — it acquires the wake lock and waits for the run
		// loop to connect. Since we have no run loop here, the
		// spawned task will eventually time out; the dispatcher
		// itself returns immediately. Cancel the camera's token so
		// the spawned task exits cleanly at the end of the test.
		let cameras = test_cameras("cam-wake");
		let mqtt = SharedMqttClient::for_test_stub("dispatch-wake");
		let cmd = ControlCommand::Wakeup {
			camera: "cam-wake".to_string(),
			minutes: 1,
		};
		dispatch_control(cmd, &cameras, &mqtt, "bairelay").await;
		// Cancel so the background task exits — otherwise it would
		// linger for the 120 s connect-timeout budget.
		cameras.get("cam-wake").unwrap().cancel_token().cancel();
		tokio::time::sleep(Duration::from_millis(20)).await;
	}

	/// `ControlCommand::Reboot` dispatches to `CameraDriver::reboot`
	/// exactly once. Fake records the invocation count; the OK reply
	/// is published on the control topic.
	#[tokio::test]
	async fn dispatch_reboot_invokes_driver_and_replies_ok() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-reboot", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Reboot {
				camera: "cam-reboot".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(
			*fake.calls().reboot.lock().unwrap(),
			1,
			"reboot should be invoked exactly once"
		);
		assert!(
			mock.published()
				.iter()
				.any(|(t, p, _)| { t == "bairelay/cam-reboot/control/reboot" && p == b"OK" }),
			"OK reply should be published on the control topic; observed: {:?}",
			mock.published_topics()
		);
	}

	/// `ControlCommand::Floodlight { state: true }` dispatches to
	/// `set_floodlight_manual(true, 30)` — the hard-coded 30-second
	/// duration is part of the current contract and must not drift.
	#[tokio::test]
	async fn dispatch_floodlight_on_invokes_driver_with_30s_duration() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-fl", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Floodlight {
				camera: "cam-fl".to_string(),
				state: true,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(
			*fake.calls().set_floodlight_manual.lock().unwrap(),
			vec![(true, 30)],
			"Floodlight on should call set_floodlight_manual(true, 30)"
		);
	}

	/// `ControlCommand::Floodlight { state: false }` — same path,
	/// opposite state.
	#[tokio::test]
	async fn dispatch_floodlight_off_invokes_driver_with_30s_duration() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-fl-off", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Floodlight {
				camera: "cam-fl-off".to_string(),
				state: false,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(
			*fake.calls().set_floodlight_manual.lock().unwrap(),
			vec![(false, 30)]
		);
	}

	/// `ControlCommand::Pir { state: true }` dispatche sur `pir_set(true)`,
	/// **relit** le matériel, et republie `status/pir` d'après la relecture.
	/// Avant le correctif, l'état republié était `state` : la publication
	/// disait « ce que j'ai voulu », et survivait en topic conservé.
	#[tokio::test]
	async fn dispatch_pir_set_invokes_driver_and_republishes_state() {
		let fake = FakeCameraBuilder::new()
			.with_pirstate(|| Ok(pir_cfg(1, Some(2))))
			.build();
		let (cameras, fake) = test_cameras_with_fake("cam-pir", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Pir {
				camera: "cam-pir".to_string(),
				state: true,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(*fake.calls().pir_set.lock().unwrap(), vec![true]);
		// Both the status republish and the OK reply should appear.
		let rows = mock.published();
		assert!(
			rows.iter()
				.any(|(t, p, r)| t == "bairelay/cam-pir/status/pir" && p == b"on" && *r),
			"Pir set should re-publish retained status/pir=on; observed: {:?}",
			rows
		);
		assert!(
			rows.iter()
				.any(|(t, p, _)| t == "bairelay/cam-pir/control/pir" && p == b"OK"),
			"Pir set should reply OK on the control topic; observed: {:?}",
			rows
		);
	}

	/// `ControlCommand::PtzPresetByName` with a name that isn't in the
	/// preset cache must publish `FAIL` on the reply topic, not `OK`.
	/// Previously this returned `Ok(())` and operators saw a successful
	/// reply while the camera never moved.
	#[tokio::test]
	async fn dispatch_ptz_preset_by_name_cache_miss_replies_fail() {
		// `cam.preset_id_for_name(...)` returns `None` for any name that
		// hasn't been populated by `replace_preset_cache`. A fresh
		// FakeCamera-backed CameraHandle has an empty cache.
		let fake = FakeCameraBuilder::new().build();
		let (cameras, _fake) = test_cameras_with_fake("cam-pn", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::PtzPresetByName {
				camera: "cam-pn".to_string(),
				name: "Doorstep".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		// The reply topic mirrors the inbound command topic; cache-miss
		// must produce a non-retained `FAIL`.
		let publishes = mock.published();
		let reply = publishes
			.iter()
			.find(|(t, _, _)| t == "bairelay/cam-pn/control/ptz/preset")
			.expect("dispatcher must publish on the reply topic");
		assert_eq!(reply.1, b"FAIL", "cache-miss must reply FAIL");
	}

	/// `ControlCommand::PtzAssign { preset_id: 1, name: "Home" }`
	/// dispatches to `set_ptz_preset(1, "Home")`.
	#[tokio::test]
	async fn dispatch_ptz_assign_invokes_driver_with_id_and_name() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-pt", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::PtzAssign {
				camera: "cam-pt".to_string(),
				preset_id: 1,
				name: "Home".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(
			*fake.calls().set_ptz_preset.lock().unwrap(),
			vec![(1u8, "Home".to_string())]
		);
	}

	/// `ControlCommand::Ptz { direction: Up, amount: 0.0 }` dispatches to
	/// `send_ptz` twice: first with the requested direction+speed, then
	/// unconditionally with `Direction::Stop` so the camera halts even if
	/// the initial move errored. Amount 0.0 keeps the sleep duration at
	/// zero so the test doesn't wait on a real clock.
	#[tokio::test]
	async fn dispatch_ptz_directional_move_sends_direction_then_stop() {
		use bairelay_neolink_core::bc_protocol::Direction;

		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-pd", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Ptz {
				camera: "cam-pd".to_string(),
				direction: PtzDirection::Up,
				amount: 0.0,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let calls = fake.calls().send_ptz.lock().unwrap().clone();
		assert_eq!(
			calls,
			vec![(Direction::Up, 32.0_f32), (Direction::Stop, 32.0_f32)],
			"directional Ptz should issue the direction then an unconditional Stop"
		);
	}

	/// Even when the initial `send_ptz(direction)` errors, the dispatch
	/// must still call `send_ptz(Stop)` — a camera that moved partway
	/// before the command failed should not be left running.
	#[tokio::test]
	async fn dispatch_ptz_directional_still_stops_on_error() {
		use bairelay_neolink_core::bc_protocol::Direction;

		// FakeCamera::send_ptz always returns Ok in the scaffolding, so
		// simulate error via a single-attempt counter-closure is not
		// possible without new FakeCamera plumbing. Instead we test the
		// always-stop invariant indirectly: a directional move must
		// record exactly two send_ptz calls regardless of amount,
		// because the second (Stop) is unconditional.
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-pe", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Ptz {
				camera: "cam-pe".to_string(),
				direction: PtzDirection::Left,
				amount: 0.0,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let calls = fake.calls().send_ptz.lock().unwrap().clone();
		assert_eq!(calls.len(), 2, "must record two send_ptz calls");
		assert_eq!(calls[1].0, Direction::Stop, "second call must be Stop");
	}

	/// `ControlCommand::PtzPreset { preset_id: 3 }` dispatches to
	/// `moveto_ptz_preset(3)`.
	#[tokio::test]
	async fn dispatch_ptz_preset_invokes_driver_moveto() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-pm", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::PtzPreset {
				camera: "cam-pm".to_string(),
				preset_id: 3,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(*fake.calls().moveto_ptz_preset.lock().unwrap(), vec![3u8]);
	}

	/// `ControlCommand::Led { state: true }` dispatches to `led_light_set(true)`.
	#[tokio::test]
	async fn dispatch_led_on_invokes_driver() {
		let fake = FakeCameraBuilder::new()
			.with_ledstate(|| Ok(led_state("auto", "open")))
			.build();
		let (cameras, fake) = test_cameras_with_fake("cam-led", fake);
		with_led_hardware(&cameras["cam-led"]);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Led {
				camera: "cam-led".to_string(),
				state: true,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(*fake.calls().led_light_set.lock().unwrap(), vec![true]);
		let rows = mock.published();
		assert!(rows
			.iter()
			.any(|(t, p, _)| t == "bairelay/cam-led/control/led" && p == b"OK"));
		assert!(
			rows.iter()
				.any(|(t, p, r)| t == "bairelay/cam-led/status/led" && p == b"on" && *r),
			"status/led must be republished; observed: {rows:?}"
		);
		assert_eq!(cameras["cam-led"].status_cache().led(), Some(true));
	}

	/// `ControlCommand::Ir` maps each IrMode variant onto the matching
	/// `LightState` and calls `irled_light_set`.
	#[tokio::test]
	async fn dispatch_ir_maps_each_mode_variant() {
		use bairelay_neolink_core::bc_protocol::LightState;
		let cases = [
			(IrMode::On, LightState::On),
			(IrMode::Off, LightState::Off),
			(IrMode::Auto, LightState::Auto),
		];
		for (mode, expected) in cases {
			// Re-read after the write reports the requested mode.
			let cam_state = match mode {
				IrMode::On => "open",
				IrMode::Off => "close",
				IrMode::Auto => "auto",
			};
			let fake = FakeCameraBuilder::new()
				.with_ledstate(move || Ok(led_state(cam_state, "close")))
				.build();
			let (cameras, fake) = test_cameras_with_fake("cam-ir", fake);
			with_led_hardware(&cameras["cam-ir"]);
			let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
			dispatch_control(
				ControlCommand::Ir {
					camera: "cam-ir".to_string(),
					mode,
				},
				&cameras,
				&mqtt,
				"bairelay",
			)
			.await;
			let calls = fake.calls().irled_light_set.lock().unwrap().clone();
			assert_eq!(calls.len(), 1, "one irled_light_set call per dispatch");
			assert_eq!(calls[0], expected);
			// New mode republished on status/ir (retained) and cached.
			assert!(mock
				.published()
				.iter()
				.any(|(t, p, r)| t == "bairelay/cam-ir/status/ir"
					&& p == mode.as_payload().as_bytes()
					&& *r));
			assert_eq!(cameras["cam-ir"].status_cache().ir(), Some(mode));
		}
	}

	use crate::capabilities::Presence;

	/// Mark both status LED and IR LEDs as confirmed hardware.
	fn with_led_hardware(cam: &CameraHandle) {
		cam.set_capabilities_for_test(crate::capabilities::CameraCapabilities {
			status_led: Presence::Present,
			ir_leds: Presence::Present,
			..Default::default()
		});
	}

	/// Presence unknown (capabilities never probed, or the probe gave
	/// no signal): commands still reach the camera, but the re-read
	/// may be a placeholder, so no state is published or cached.
	#[tokio::test]
	async fn dispatch_led_and_ir_with_unknown_presence_publish_no_state() {
		let fake = FakeCameraBuilder::new()
			.with_ledstate(|| Ok(led_state("close", "open")))
			.build();
		let (cameras, fake) = test_cameras_with_fake("cam-unk", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let led = ControlCommand::Led {
			camera: "cam-unk".to_string(),
			state: true,
		};
		let ir = ControlCommand::Ir {
			camera: "cam-unk".to_string(),
			mode: IrMode::Auto,
		};
		dispatch_control(led, &cameras, &mqtt, "bairelay").await;
		dispatch_control(ir, &cameras, &mqtt, "bairelay").await;
		assert_eq!(fake.calls().led_light_set.lock().unwrap().len(), 1);
		assert_eq!(fake.calls().irled_light_set.lock().unwrap().len(), 1);
		assert!(status_rows(&mock, "bairelay/cam-unk/status/led").is_empty());
		assert!(status_rows(&mock, "bairelay/cam-unk/status/ir").is_empty());
		let cache = cameras["cam-unk"].status_cache();
		assert_eq!((cache.led(), cache.ir()), (None, None));
	}

	/// Camera confirmed to lack a status LED: `control/led` still goes
	/// out, but no `status/led` is published.
	#[tokio::test]
	async fn dispatch_led_on_camera_without_status_led_publishes_no_state() {
		let fake = FakeCameraBuilder::new()
			.with_ledstate(|| Ok(led_state("auto", "open")))
			.build();
		let (cameras, fake) = test_cameras_with_fake("cam-noled", fake);
		cameras["cam-noled"].set_capabilities_for_test(crate::capabilities::CameraCapabilities {
			status_led: Presence::Absent,
			ir_leds: Presence::Present,
			..Default::default()
		});
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let cmd = ControlCommand::Led {
			camera: "cam-noled".to_string(),
			state: false,
		};
		dispatch_control(cmd, &cameras, &mqtt, "bairelay").await;
		assert_eq!(fake.calls().led_light_set.lock().unwrap().len(), 1);
		assert!(status_rows(&mock, "bairelay/cam-noled/status/led").is_empty());
	}

	fn led_state(ir: &str, light: &str) -> bairelay_neolink_core::bc::xml::LedState {
		bairelay_neolink_core::bc::xml::LedState {
			state: ir.into(),
			light_state: light.into(),
			..Default::default()
		}
	}

	fn status_rows(mock: &bairelay_mqtt::test_support::MockHandle, topic: &str) -> Vec<Vec<u8>> {
		mock.published()
			.into_iter()
			.filter(|(t, _, _)| t == topic)
			.map(|(_, p, _)| p)
			.collect()
	}

	/// The camera answers OK but ignores the write: bairelay publishes
	/// what the post-write re-read reports, not what was requested.
	#[tokio::test]
	async fn dispatch_ir_publishes_camera_state_not_requested_mode() {
		let fake = FakeCameraBuilder::new()
			.with_ledstate(|| Ok(led_state("auto", "close")))
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-ir", fake);
		with_led_hardware(&cameras["cam-ir"]);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let cmd = ControlCommand::Ir {
			camera: "cam-ir".to_string(),
			mode: IrMode::On,
		};
		dispatch_control(cmd, &cameras, &mqtt, "bairelay").await;
		assert_eq!(
			status_rows(&mock, "bairelay/cam-ir/status/ir"),
			vec![b"auto".to_vec()]
		);
		assert_eq!(cameras["cam-ir"].status_cache().ir(), Some(IrMode::Auto));
	}

	/// Re-read fails after a successful write: fall back to publishing
	/// the requested value.
	#[tokio::test]
	async fn dispatch_led_falls_back_to_requested_when_reread_fails() {
		let fake = FakeCameraBuilder::new()
			.with_ledstate(|| {
				Err(bairelay_neolink_core::bc_protocol::Error::Other(
					"reread refused",
				))
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-led", fake);
		with_led_hardware(&cameras["cam-led"]);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let cmd = ControlCommand::Led {
			camera: "cam-led".to_string(),
			state: false,
		};
		dispatch_control(cmd, &cameras, &mqtt, "bairelay").await;
		assert_eq!(
			status_rows(&mock, "bairelay/cam-led/status/led"),
			vec![b"off".to_vec()]
		);
		assert_eq!(cameras["cam-led"].status_cache().led(), Some(false));
	}

	/// Camera known to lack IR LEDs: `control/ir` still goes to the
	/// camera, but the placeholder `state` it reads back isn't published.
	#[tokio::test]
	async fn dispatch_ir_on_camera_without_ir_leds_publishes_no_state() {
		let fake = FakeCameraBuilder::new()
			.with_ledstate(|| Ok(led_state("close", "close")))
			.build();
		let (cameras, fake) = test_cameras_with_fake("cam-noir", fake);
		cameras["cam-noir"].set_capabilities_for_test(crate::capabilities::CameraCapabilities {
			status_led: Presence::Present,
			ir_leds: Presence::Absent,
			..Default::default()
		});
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let cmd = ControlCommand::Ir {
			camera: "cam-noir".to_string(),
			mode: IrMode::Off,
		};
		dispatch_control(cmd, &cameras, &mqtt, "bairelay").await;
		assert_eq!(fake.calls().irled_light_set.lock().unwrap().len(), 1);
		assert!(status_rows(&mock, "bairelay/cam-noir/status/ir").is_empty());
		assert_eq!(cameras["cam-noir"].status_cache().ir(), None);
	}

	/// A failed write publishes no state, leaves the cache alone, and
	/// replies FAIL.
	#[tokio::test]
	async fn dispatch_failed_led_and_ir_writes_publish_nothing() {
		let fake = FakeCameraBuilder::new().with_led_writes_failing().build();
		let (cameras, _fake) = test_cameras_with_fake("cam-x", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let led = ControlCommand::Led {
			camera: "cam-x".to_string(),
			state: true,
		};
		let ir = ControlCommand::Ir {
			camera: "cam-x".to_string(),
			mode: IrMode::Off,
		};
		dispatch_control(led, &cameras, &mqtt, "bairelay").await;
		dispatch_control(ir, &cameras, &mqtt, "bairelay").await;
		assert!(status_rows(&mock, "bairelay/cam-x/status/led").is_empty());
		assert!(status_rows(&mock, "bairelay/cam-x/status/ir").is_empty());
		let cache = cameras["cam-x"].status_cache();
		assert_eq!((cache.led(), cache.ir()), (None, None));
		assert_eq!(
			status_rows(&mock, "bairelay/cam-x/control/led"),
			vec![b"FAIL".to_vec()]
		);
	}

	/// `Siren { state: true }` invokes `siren()`; `state: false` is a no-op
	/// (the command still replies `OK`).
	#[tokio::test]
	async fn dispatch_siren_on_invokes_driver_and_off_is_noop() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-siren", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::Siren {
				camera: "cam-siren".to_string(),
				state: true,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert_eq!(*fake.calls().siren.lock().unwrap(), 1);

		// Off path is an explicit Ok(()) — no siren() call.
		dispatch_control(
			ControlCommand::Siren {
				camera: "cam-siren".to_string(),
				state: false,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert_eq!(
			*fake.calls().siren.lock().unwrap(),
			1,
			"off-path must not invoke siren()"
		);
	}

	/// `FloodlightTasks { state: true }` dispatches to
	/// `floodlight_tasks_enable(true)`.
	#[tokio::test]
	async fn dispatch_floodlight_tasks_invokes_driver() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-ft", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::FloodlightTasks {
				camera: "cam-ft".to_string(),
				state: true,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert_eq!(
			*fake.calls().floodlight_tasks_enable.lock().unwrap(),
			vec![true]
		);
	}

	/// `Zoom { level }` dispatches to `zoom_to((level*1000) as u32)`.
	#[tokio::test]
	async fn dispatch_zoom_scales_level_to_zoom_to() {
		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-zoom", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::Zoom {
				camera: "cam-zoom".to_string(),
				level: 0.5,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert_eq!(*fake.calls().zoom_to.lock().unwrap(), vec![500u32]);
	}

	/// `QueryBattery` calls `battery_info()` and publishes a
	/// serialized XML payload to `status/battery`.
	#[tokio::test]
	async fn dispatch_query_battery_publishes_xml() {
		use bairelay_neolink_core::bc::xml::BatteryInfo;
		let fake = FakeCameraBuilder::new()
			.with_battery_info(|| {
				Ok(BatteryInfo {
					battery_percent: 42,
					..Default::default()
				})
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qb", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryBattery {
				camera: "cam-qb".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert!(mock
			.published()
			.iter()
			.any(|(t, p, _)| t == "bairelay/cam-qb/status/battery"
				&& std::str::from_utf8(p).unwrap_or("").contains("42")));
	}

	/// `QueryBattery` with a driver error — the dispatch still completes,
	/// replies OK on the control topic, and no status/battery row is
	/// published.
	#[tokio::test]
	async fn dispatch_query_battery_handles_driver_error() {
		let fake = FakeCameraBuilder::new()
			.with_battery_info(|| {
				Err(bairelay_neolink_core::bc_protocol::Error::Other(
					"battery refused",
				))
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qbe", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryBattery {
				camera: "cam-qbe".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert!(
			!mock
				.published()
				.iter()
				.any(|(t, _, _)| t == "bairelay/cam-qbe/status/battery"),
			"error path must not publish status/battery"
		);
	}

	/// `QueryPreview` asks the driver for a snapshot and publishes the
	/// bytes on `status/preview`.
	#[tokio::test]
	async fn dispatch_query_preview_publishes_jpeg() {
		let jpeg: Vec<u8> = {
			// Minimal valid JPEG so the overlay renderer (if on) doesn't
			// crash — the dispatch path runs overlay only when
			// `preview_overlay` is true in config, which our
			// minimal_camera_config defaults it to whatever PauseConfig
			// defaults are; render() on non-Live state would need a
			// decodable image. Use a small synthetic RGB encoded as JPEG.
			let img = image::RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 0]));
			let mut out = Vec::new();
			image::DynamicImage::ImageRgb8(img)
				.write_to(
					&mut std::io::Cursor::new(&mut out),
					image::ImageFormat::Jpeg,
				)
				.expect("encode");
			out
		};
		let jpeg_clone = jpeg.clone();
		let fake = FakeCameraBuilder::new()
			.with_snapshot(move || Ok(jpeg_clone.clone()))
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qp", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryPreview {
				camera: "cam-qp".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		// Preview poller initial state is Sleeping — overlay ON will
		// render text atop the JPEG so bytes differ from input; we only
		// assert a publish on the preview topic happened.
		assert!(
			mock.published()
				.iter()
				.any(|(t, _, _)| t == "bairelay/cam-qp/status/preview"),
			"QueryPreview should publish status/preview; observed: {:?}",
			mock.published_topics()
		);
	}

	/// `QueryPreview` with a driver error — the dispatch still finishes
	/// without panicking and nothing is published on `status/preview`.
	#[tokio::test]
	async fn dispatch_query_preview_handles_driver_error() {
		let fake = FakeCameraBuilder::new()
			.with_snapshot(|| Err(bairelay_neolink_core::bc_protocol::Error::Other("no snap")))
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qpe", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryPreview {
				camera: "cam-qpe".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert!(!mock
			.published()
			.iter()
			.any(|(t, _, _)| t == "bairelay/cam-qpe/status/preview"));
	}

	/// `QueryPreview` with `preview_overlay = false` takes the
	/// passthrough branch (line 211) — raw JPEG is republished as-is.
	#[tokio::test]
	async fn dispatch_query_preview_passthrough_when_overlay_disabled() {
		let jpeg: Vec<u8> = {
			let img = image::RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 0]));
			let mut out = Vec::new();
			image::DynamicImage::ImageRgb8(img)
				.write_to(
					&mut std::io::Cursor::new(&mut out),
					image::ImageFormat::Jpeg,
				)
				.expect("encode");
			out
		};
		let jpeg_clone = jpeg.clone();
		let fake = FakeCameraBuilder::new()
			.with_snapshot(move || Ok(jpeg_clone.clone()))
			.build();

		let cancel = CancellationToken::new();
		let mut cfg = minimal_camera_config("cam-qpov");
		cfg.pause.preview_overlay = false;
		let handle = Arc::new(CameraHandle::new(cfg, cancel, None));
		let driver: Arc<dyn CameraDriver> = fake;
		handle.set_driver_for_test(driver);
		let mut cameras = HashMap::new();
		cameras.insert("cam-qpov".to_string(), handle);

		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryPreview {
				camera: "cam-qpov".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert!(mock
			.published()
			.iter()
			.any(|(t, _, _)| t == "bairelay/cam-qpov/status/preview"));
	}

	/// `QueryPir` publishes the camera's PIR state on `status/pir`.
	#[tokio::test]
	async fn dispatch_query_pir_publishes_xml() {
		use bairelay_neolink_core::bc::xml::RfAlarmCfg;
		let fake = FakeCameraBuilder::new()
			.with_pirstate(|| {
				Ok(RfAlarmCfg {
					enable: 1,
					..Default::default()
				})
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qpir", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryPir {
				camera: "cam-qpir".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert!(mock
			.published()
			.iter()
			.any(|(t, _, _)| t == "bairelay/cam-qpir/status/pir"));
	}

	/// `QueryPir` with a driver error — dispatch returns without
	/// publishing anything to `status/pir`.
	#[tokio::test]
	async fn dispatch_query_pir_handles_driver_error() {
		let fake = FakeCameraBuilder::new()
			.with_pirstate(|| {
				Err(bairelay_neolink_core::bc_protocol::Error::Other(
					"pir refused",
				))
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qpe", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryPir {
				camera: "cam-qpe".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert!(!mock
			.published()
			.iter()
			.any(|(t, _, _)| t == "bairelay/cam-qpe/status/pir"));
	}

	/// `QueryPtzPreset` re-queries the camera, replaces the cached
	/// preset list, and replies OK on the control topic. The HA
	/// discovery republish is exercised via `publish_discovery`; with
	/// no `DiscoveryPublisher` attached on the test handle that call
	/// is a silent no-op so we only assert the cache + reply here.
	#[tokio::test]
	async fn dispatch_query_ptz_preset_refreshes_cache_and_replies_ok() {
		use bairelay_neolink_core::bc::xml::{Preset, PresetList, PtzPreset};
		let fake = FakeCameraBuilder::new()
			.with_ptz_preset(|| {
				Ok(PtzPreset {
					preset_list: PresetList {
						preset: vec![
							Preset {
								id: 1,
								name: Some("Garden".to_string()),
								..Default::default()
							},
							Preset {
								id: 2,
								name: Some("Driveway".to_string()),
								..Default::default()
							},
						],
					},
					..Default::default()
				})
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qptz", fake);
		// Seed a stale cache so we can prove the handler replaced it.
		cameras
			.get("cam-qptz")
			.unwrap()
			.set_preset_cache_for_test(vec![(99, "stale".to_string())]);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryPtzPreset {
				camera: "cam-qptz".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert_eq!(
			cameras.get("cam-qptz").unwrap().preset_cache(),
			vec![(1u8, "Garden".to_string()), (2u8, "Driveway".to_string())],
			"QueryPtzPreset must replace the cache with fresh camera data"
		);
		assert!(mock
			.published()
			.iter()
			.any(|(t, p, _)| t == "bairelay/cam-qptz/query/ptz/preset" && p == b"OK"));
	}

	/// When `get_ptz_preset` returns an error, the handler logs and
	/// leaves the existing cache intact (no clobber to empty).
	#[tokio::test]
	async fn dispatch_query_ptz_preset_driver_error_preserves_cache() {
		let fake = FakeCameraBuilder::new()
			.with_ptz_preset(|| {
				Err(bairelay_neolink_core::bc_protocol::Error::Other(
					"ptz refused",
				))
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qptze", fake);
		cameras
			.get("cam-qptze")
			.unwrap()
			.set_preset_cache_for_test(vec![(7, "Front".to_string())]);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		dispatch_control(
			ControlCommand::QueryPtzPreset {
				camera: "cam-qptze".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;
		assert_eq!(
			cameras.get("cam-qptze").unwrap().preset_cache(),
			vec![(7u8, "Front".to_string())],
			"driver error must not clobber the previously-cached presets"
		);
		assert!(mock
			.published()
			.iter()
			.any(|(t, p, _)| t == "bairelay/cam-qptze/query/ptz/preset" && p == b"OK"));
	}

	/// When `get_ptz_preset` hangs past `CMD_TIMEOUT` the handler logs
	/// and replies OK; the cache is left intact.
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn dispatch_query_ptz_preset_timeout_path() {
		let fake = FakeCameraBuilder::new().with_ptz_preset_pending().build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qptzt", fake);
		cameras
			.get("cam-qptzt")
			.unwrap()
			.set_preset_cache_for_test(vec![(3, "Side".to_string())]);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();
		let dispatch = dispatch_control(
			ControlCommand::QueryPtzPreset {
				camera: "cam-qptzt".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		);
		let (_, _) = tokio::join!(dispatch, async {
			tokio::time::advance(Duration::from_secs(31)).await;
		});
		assert_eq!(
			cameras.get("cam-qptzt").unwrap().preset_cache(),
			vec![(3u8, "Side".to_string())],
			"timeout must not clobber the previously-cached presets"
		);
	}

	/// Directional PTZ where the move hits a timeout (amount > 10 * 32
	/// would normally just cap at 10s sleep; instead we put enough
	/// budget pressure by ... actually we just exercise the successful
	/// move with amount > 0 so the sleep branch runs.)
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn dispatch_ptz_with_positive_amount_sleeps_before_stop() {
		use bairelay_neolink_core::bc_protocol::Direction;

		let fake = FakeCameraBuilder::new().build();
		let (cameras, fake) = test_cameras_with_fake("cam-pds", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();

		// amount = 32 → seconds = 32/32 = 1s of sleep between dir+stop.
		dispatch_control(
			ControlCommand::Ptz {
				camera: "cam-pds".to_string(),
				direction: PtzDirection::Right,
				amount: 32.0,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let calls = fake.calls().send_ptz.lock().unwrap().clone();
		assert_eq!(
			calls,
			vec![(Direction::Right, 32.0_f32), (Direction::Stop, 32.0_f32)]
		);
	}

	/// `QueryBattery` whose driver hangs past the 30 s `CMD_TIMEOUT`
	/// budget — the dispatcher must log the timeout and return without
	/// publishing anything on `status/battery`.
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn dispatch_query_battery_timeout_path() {
		let fake = FakeCameraBuilder::new().with_battery_info_pending().build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qbt", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let dispatch = dispatch_control(
			ControlCommand::QueryBattery {
				camera: "cam-qbt".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		);
		// Run the dispatcher and the clock-advance task concurrently so
		// the timeout future fires while we await the dispatcher.
		let (_, _) = tokio::join!(dispatch, async {
			tokio::time::advance(Duration::from_secs(31)).await;
		});
		assert!(
			!mock
				.published()
				.iter()
				.any(|(t, _, _)| t == "bairelay/cam-qbt/status/battery"),
			"timeout path must NOT publish status/battery"
		);
	}

	/// `QueryPir` whose driver hangs past 30 s — same expectation.
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn dispatch_query_pir_timeout_path() {
		let fake = FakeCameraBuilder::new().with_pirstate_pending().build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qpit", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let dispatch = dispatch_control(
			ControlCommand::QueryPir {
				camera: "cam-qpit".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		);
		let (_, _) = tokio::join!(dispatch, async {
			tokio::time::advance(Duration::from_secs(31)).await;
		});
		assert!(!mock
			.published()
			.iter()
			.any(|(t, _, _)| t == "bairelay/cam-qpit/status/pir"));
	}

	/// `QueryPreview` whose driver hangs past 30 s — no preview publish.
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn dispatch_query_preview_timeout_path() {
		let fake = FakeCameraBuilder::new().with_snapshot_pending().build();
		let (cameras, _fake) = test_cameras_with_fake("cam-qpvt", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let dispatch = dispatch_control(
			ControlCommand::QueryPreview {
				camera: "cam-qpvt".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		);
		let (_, _) = tokio::join!(dispatch, async {
			tokio::time::advance(Duration::from_secs(31)).await;
		});
		assert!(!mock
			.published()
			.iter()
			.any(|(t, _, _)| t == "bairelay/cam-qpvt/status/preview"));
	}

	/// PTZ where `send_ptz(direction)` hangs past the 30 s timeout —
	/// dispatcher logs the timeout, then issues the unconditional Stop.
	/// Both calls land in the call log because the pending fake
	/// records the call before suspending.
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn dispatch_ptz_directional_timeout_still_stops() {
		use bairelay_neolink_core::bc_protocol::Direction;
		let fake = FakeCameraBuilder::new().with_send_ptz_pending().build();
		let (cameras, fake) = test_cameras_with_fake("cam-ptt", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();
		let dispatch = dispatch_control(
			ControlCommand::Ptz {
				camera: "cam-ptt".to_string(),
				direction: PtzDirection::Down,
				amount: 0.0,
			},
			&cameras,
			&mqtt,
			"bairelay",
		);
		// First send_ptz timeout, then unconditional stop call ALSO
		// times out. Advance well past 60 s so both fire.
		let (_, _) = tokio::join!(dispatch, async {
			tokio::time::advance(Duration::from_secs(70)).await;
		});
		// Both directional and stop calls were recorded on entry to
		// `send_ptz` (before the pending await).
		let calls = fake.calls().send_ptz.lock().unwrap().clone();
		assert_eq!(calls.len(), 2, "both directional and stop calls recorded");
		assert_eq!(calls[0].0, Direction::Down);
		assert_eq!(calls[1].0, Direction::Stop);
	}

	/// PTZ where `send_ptz(direction)` returns Err — dispatcher logs the
	/// inner-error warn (line 119-120) and still issues the Stop. Use a
	/// fake variant where send_ptz fails — current FakeCamera always
	/// returns Ok, so we skip this and rely on the pending-timeout path
	/// to cover the warn-then-stop arm.
	///
	/// Siren whose driver hangs past 30 s — dispatcher should still
	/// reply OK on the control topic since `state: true` is forwarded to
	/// `bc.siren()` whose timeout returns the normalised error.
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn dispatch_siren_timeout_path() {
		let fake = FakeCameraBuilder::new().with_siren_pending().build();
		let (cameras, fake) = test_cameras_with_fake("cam-st", fake);
		let (mqtt, _mock) = bairelay_mqtt::test_support::mock_client();
		let dispatch = dispatch_control(
			ControlCommand::Siren {
				camera: "cam-st".to_string(),
				state: true,
			},
			&cameras,
			&mqtt,
			"bairelay",
		);
		let (_, _) = tokio::join!(dispatch, async {
			tokio::time::advance(Duration::from_secs(31)).await;
		});
		// siren() was entered (and the call recorded) before pending.
		assert_eq!(*fake.calls().siren.lock().unwrap(), 1);
	}

	#[test]
	fn serialize_xml_round_trips_a_struct() {
		#[derive(serde::Serialize)]
		#[serde(rename = "Battery")]
		struct Battery {
			percent: u8,
		}
		let s = serialize_xml(&Battery { percent: 42 }).expect("serialize ok");
		assert!(s.contains("percent"));
		assert!(s.contains("42"));
	}

	// ── Réglages : la loi de la relecture ──────────────────────────────
	//
	// Tous ces tests branchent le dispatch sur une caméra **simulée** :
	// `MockConnection` est un channel en mémoire (aucune prise réseau, aucune
	// trame sur un socket), et `FakeCamera` un pilote entièrement scripté.
	// Le scénario central est le même partout : la caméra acquitte l'écriture
	// en 200, et ce qu'elle répond à la relecture qui suit n'est pas ce qui
	// avait dst demandé.

	use bairelay_mqtt::settings::{Family, SettingCommand};
	use bairelay_neolink_core::bc::model::{
		Bc, MSG_ID_DING_DONG_CTRL, MSG_ID_GET_AUDIO_CFG, MSG_ID_GET_DING_DONG_CFG,
		MSG_ID_GET_DING_DONG_LIST, MSG_ID_GET_IMAGE, MSG_ID_GET_MOTION_DETECT, MSG_ID_GET_NTP,
		MSG_ID_GET_OSD, MSG_ID_GET_PIR_ALARM, MSG_ID_SET_AUDIO_CFG, MSG_ID_START_PIR_ALARM,
	};
	use bairelay_neolink_core::bc::xml::{
		AudioCfg, BcXml, MdAlarm, Ntp, OsdDatetime, RfAlarmCfg, SensInfoNew, VideoInput,
	};
	use bairelay_neolink_core::bc_protocol::connection::mock::{
		reply_200_empty, reply_200_xml, reply_err_code, MockConnection,
	};
	use bairelay_neolink_core::bc_protocol::BcCamera;

	/// Un `<RfAlarmCfg>` tel que l'imprime une sonnette en protocole v1.1 : la
	/// sensibilité s'appelle `sensiValue` (et non `sensitivity`), et l'octet
	/// `enable` est ce que le matériel croit vraiment être son état.
	fn pir_cfg(enable: u8, sensi: Option<u8>) -> RfAlarmCfg {
		RfAlarmCfg {
			version: "1.1".to_string(),
			enable,
			sensiValue: sensi,
			..Default::default()
		}
	}

	/// Le même bloc, dans l'orthographe v1.1 qui porte `<interval>` : le temps
	/// de repos entre deux alarmes PIR, en secondes, et les bornes que le
	/// firmware annonce pour ce curseur.
	fn pir_cfg_with_interval(repos: u32, max: u32) -> RfAlarmCfg {
		RfAlarmCfg {
			interval: Some(repos),
			intervalSecMin: Some(5),
			intervalSecMax: Some(max),
			..pir_cfg(1, Some(21))
		}
	}

	/// Une réponse 200 du msg 212 : le bloc dans `<RfAlarmCfg>`, comme le
	/// renvoie un vrai firmware.
	fn answer_pir(req: &Bc, bloc: RfAlarmCfg) -> Bc {
		reply_200_xml(
			req,
			BcXml {
				rf_alarm_cfg: Some(bloc),
				..Default::default()
			},
		)
	}

	/// Installe un vrai [`BcCamera`] branché sur une connexion scriptée, dans
	/// les deux slots du handle — exactement ce que fait une session ouverte.
	/// Les réglages lisent le slot concret, les commandes historiques le slot
	/// du trait : les deux doivent être cohérents.
	async fn cameras_with_mock(
		name: &str,
		mock: MockConnection,
	) -> HashMap<String, Arc<CameraHandle>> {
		let cam = Arc::new(BcCamera::from_mock_connection(mock).await);
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config(name),
			CancellationToken::new(),
			None,
		));
		handle.set_bc_camera_for_test(cam);
		let mut map = HashMap::new();
		map.insert(name.to_string(), handle);
		map
	}

	/// Le corps d'un test de réglages, épinglé sur le tas pour traverser un fil
	/// sans fioritures de generics.
	type CorpsDeTest = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

	/// Fait tourner un corps de test sur un fil à **pile explicite**, avec son
	/// propre runtime.
	///
	/// Pourquoi ce détour : `BcXml`, c'est tous les blocs du protocole en
	/// `Option`, plusieurs kilo-octets, et chaque commande caméra en brasse au
	/// moins deux copies. Empiler `dispatch_control` par-dessus une
	/// `BcCamera`, c'est la chaîne la plus gourmande en pile du pont. En
	/// release — le profil du binaire qui tourne chez l'utilisatrice — le fil
	/// reçoit exactement les **2 Mo d'un worker tokio**, la pile dont dispose
	/// vraiment `dispatch_control` puisque `mqtt_loop` l'englobe dans un
	/// `tokio::spawn` : un test vert est donc la preuve que le path tient dans
	/// la pile de la production (`cargo test --release` est la passe qui le
	/// démontre). En debug, les cadres non optimisés réclament plus large, et le
	/// test ne prouve plus que le comportement — c'est voulu, pas une cache.
	fn on_worker(body: fn() -> CorpsDeTest) {
		let stack = if cfg!(debug_assertions) {
			8 * 1024 * 1024
		} else {
			2 * 1024 * 1024
		};
		std::thread::Builder::new()
			.stack_size(stack)
			.spawn(move || {
				let rt = tokio::runtime::Builder::new_current_thread()
					.enable_time()
					.start_paused(false)
					.build()
					.expect("runtime du fil de test");
				rt.block_on(body());
			})
			.expect("fil de test à pile explicite")
			.join()
			.expect("le pont ne doit pas paniquer en relisant les réglages");
	}

	/// Le bloc `<AudioCfg>` d'une sonnette : les deux volumes d'interphone sont
	/// imprimés, et `<volume>` (le haut-parleur) reste là, lui, sans qu'on y
	/// touche — c'est lui qui prouve que le bloc repart entier.
	fn audio_cfg(talk: &str, visitor: &str) -> AudioCfg {
		AudioCfg {
			version: "1.1".to_string(),
			channel_id: "0".to_string(),
			timeout: "10".to_string(),
			audio_select: "0".to_string(),
			volume: "5".to_string(),
			pre_alarm: "0".to_string(),
			audio_list_id: "0".to_string(),
			talk_and_reply_volume: Some(talk.to_string()),
			visitor_volume: Some(visitor.to_string()),
			visitor_loudspeaker: Some("1".to_string()),
			..Default::default()
		}
	}

	fn audio_reply(req: &Bc, cfg: AudioCfg) -> Bc {
		reply_200_xml(
			req,
			BcXml {
				audio_cfg: Some(cfg),
				..Default::default()
			},
		)
	}

	/// `control/pir` : le dispatch écrit, relit, et publie la relecture — y
	/// compris quand elle contredit la commande. C'est le correctif du bug «
	/// l'état publié était l'état demandé » : avant, `status/pir` répétait
	/// `state`, et un firmware qui hochait la tête sans rien écrire était
	/// invisible, d'autant que le topic est conservé.
	#[tokio::test]
	async fn dispatch_pir_publishes_the_reread_and_not_the_request() {
		let fake = FakeCameraBuilder::new()
			// Le matériel répond « activé, sensibilité 4 » à chaque relecture,
			// quelle que soit la commande : c'est le firmware qui ignore
			// l'écriture, et il acquitte en 200.
			.with_pirstate(|| Ok(pir_cfg(1, Some(4))))
			.build();
		let (cameras, fake) = test_cameras_with_fake("cam-pir-kept", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Pir {
				camera: "cam-pir-kept".to_string(),
				state: false,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		// La commande est bien partie (le FAIL serait une autre histoire).
		assert_eq!(*fake.calls().pir_set.lock().unwrap(), vec![false]);
		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-pir-kept/control/pir" && p == b"OK"),
			"l'écriture a dst acquittée : OK attendu ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, r)| t == "bairelay/cam-pir-kept/status/pir"
				&& p == b"on"
				&& *r),
			"status/pir doit porter ce que la caméra a re-lu (on), jamais ce qu'on a demandé (off) ; observations : {rows:?}"
		);
		assert!(
			!rows
				.iter()
				.any(|(t, p, _)| t == "bairelay/cam-pir-kept/status/pir" && p == b"off"),
			"l'état demandé (off) ne doit jamais se retrouver publié ; observations : {rows:?}"
		);
		// La sensibilité a sa feuille, publiée depuis la même relecture.
		assert!(
			rows.iter().any(|(t, p, r)| t
				== "bairelay/cam-pir-kept/status/pir/sensitivity"
				&& p == b"4"
				&& *r),
			"status/pir/sensitivity doit venir de la relecture ; observations : {rows:?}"
		);
		// Le cache suit la caméra, pas la commande.
		let cam = cameras.get("cam-pir-kept").expect("caméra du test");
		assert_eq!(cam.status_cache().pir(), Some(true));
	}

	/// Une relecture qui échoue ne publie **rien**. Mieux vaut chez Home
	/// Assistant un état qui date d'hier (et dont le journal dit qu'il n'a pas
	/// pu être rafraîchi) qu'un état fabriqué avec la valeur demandée.
	#[tokio::test]
	async fn dispatch_pir_without_reread_publishes_no_state() {
		// `get_pirstate` est scripté en erreur : une caméra qui ne répond pas à
		// la relecture, ou qui la refuse.
		let fake = FakeCameraBuilder::new()
			.with_pirstate(|| {
				Err(bairelay_neolink_core::bc_protocol::Error::Other(
					"pir read refused",
				))
			})
			.build();
		let (cameras, _fake) = test_cameras_with_fake("cam-pir-nord", fake);
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Pir {
				camera: "cam-pir-nord".to_string(),
				state: true,
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-pir-nord/control/pir" && p == b"OK"),
			"la commande a dst acquittée, donc OK ; observations : {rows:?}"
		);
		assert!(
			!rows
				.iter()
				.any(|(t, _, _)| t.starts_with("bairelay/cam-pir-nord/status/pir")),
			"aucune relecture, donc aucun état publié ; observations : {rows:?}"
		);
		assert_eq!(cameras["cam-pir-nord"].status_cache().pir(), None);
	}

	/// `control/audio/volume` : lecture (264), écriture (265) du bloc patché,
	/// relecture (264), publication de la relecture. Le test vérifie aussi la
	/// forme du bloc parti sur le fil : la balise demandée changée, et
	/// `<volume>` — qu'on ne pilote pas ici — toujours à sa place.
	#[test]
	fn dispatch_setting_audio_writes_then_publishes_the_reread() {
		on_worker(|| Box::pin(audio_writes_then_rereads()));
	}

	async fn audio_writes_then_rereads() {
		let sent = StdArc::new(std::sync::Mutex::new(String::new()));
		let capture = StdArc::clone(&sent);
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_AUDIO_CFG)
			.reply_with(|req| audio_reply(req, audio_cfg("10", "7")))
			.expect_msg(MSG_ID_SET_AUDIO_CFG)
			.reply_with_xml(move |_req, xml| {
				let block = xml
					.audio_cfg
					.as_ref()
					.or(xml.audio_cfg_camel.as_ref())
					.expect("l'écriture porte un bloc audioCfg");
				*capture.lock().unwrap() = format!(
					"volume={}|talk={}|visitor={}",
					block.volume,
					block.talk_and_reply_volume.as_deref().unwrap_or("<absent>"),
					block.visitor_volume.as_deref().unwrap_or("<absent>"),
				);
				reply_200_empty(_req)
			})
			// La relecture : la caméra a gardé 42.
			.expect_msg(MSG_ID_GET_AUDIO_CFG)
			.reply_with(|req| audio_reply(req, audio_cfg("42", "7")))
			.build()
			.await;
		let cameras = cameras_with_mock("cam-audio", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Setting {
				camera: "cam-audio".to_string(),
				setting: SettingCommand::AudioVolume("42".to_string()),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(
			sent.lock().unwrap().as_str(),
			"volume=5|talk=42|visitor=7",
			"le bloc écrit doit être la réponse de la lecture avec une seule balise touchée"
		);
		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, r)| t == "bairelay/cam-audio/status/audio/volume"
				&& p == b"42"
				&& *r),
			"le volume relu est publié, conservé ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t
				== "bairelay/cam-audio/status/audio/visitor_volume"
				&& p == b"7"),
			"les feuilles voisines de la même relecture sont publiées aussi ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-audio/control/audio/volume"
				&& p == b"OK"),
			"écriture acquittée : OK ; observations : {rows:?}"
		);
	}

	/// Une écriture refusée (400) répond `FAIL`, ne publie **aucun** état, mais
	/// publie le **motif** sur `status/audio/reason` : un refus qui se tait est
	/// indistinguable d'un pont mort, et le `FAIL` du topic de commande est
	/// éphémère — personne ne le retrouve le lendemain dans Home Assistant.
	#[test]
	fn dispatch_setting_refuse_by_the_camera_not_publishes_nothing() {
		on_worker(|| Box::pin(camera_refuse_write_audio()));
	}

	async fn camera_refuse_write_audio() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_AUDIO_CFG)
			.reply_with(|req| audio_reply(req, audio_cfg("10", "7")))
			.expect_msg(MSG_ID_SET_AUDIO_CFG)
			.reply_with(|req| reply_err_code(req, 400))
			.build()
			.await;
		let cameras = cameras_with_mock("cam-audio-400", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Setting {
				camera: "cam-audio-400".to_string(),
				setting: SettingCommand::AudioVolume("42".to_string()),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-audio-400/control/audio/volume"
				&& p == b"FAIL"),
			"un 400 caméra est un FAIL ; observations : {rows:?}"
		);
		// Aucun état publié : la caméra n'a rien confirmé, donc rien ne doit
		// laisser croire que 42 est la valeur en cours. Le seul topic de la
		// famille qui ait le droit de recevoir quelque chose est le motif.
		let states: Vec<_> = rows
			.iter()
			.filter(|(t, _, _)| {
				t.starts_with("bairelay/cam-audio-400/status/") && !t.ends_with("/reason")
			})
			.collect();
		assert!(
			states.is_empty(),
			"rien n'a dst stocké, donc aucun état ; observations : {rows:?}"
		);
		let cause = rows
			.iter()
			.find(|(t, _, _)| t == "bairelay/cam-audio-400/status/audio/reason");
		let Some((_, payload, conserve)) = cause else {
			panic!("le refus doit être lisible : aucun motif publié ; observations : {rows:?}");
		};
		let texte = String::from_utf8_lossy(payload).to_string();
		assert!(*conserve, "le motif est conservé, sinon il disparaît au rechargement");
		assert!(
			texte.contains("refusé"),
			"le motif doit dire un refus, pas le taire : {texte:?}"
		);
		assert!(!texte.contains('<'), "le motif ne porte pas de XML brut : {texte:?}");
	}

	/// Une valeur refusée **avant** la caméra (hors plage, texte interdit,
	/// feuille en lecture seule) répond `FAIL` sans envoyer une seule trame.
	/// Elle publie quand même le motif : un réglage refusé ne doit plus être «
	/// juste silencieux », même quand le coupable est le pont lui-même.
	#[test]
	fn dispatch_setting_value_refused_sends_not_nothing() {
		on_worker(|| Box::pin(value_refused_nothing_envoye()));
	}

	async fn value_refused_nothing_envoye() {
		// Script vide : la moindre trame fermerait le channel de la mock
		// connexion, et la commande ne pourrait plus aboutir.
		let mock = MockConnection::new().build().await;
		let cameras = cameras_with_mock("cam-rej", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::RejectedSetting {
				camera: "cam-rej".to_string(),
				family: Family::Denoise,
				leaf: "level".to_string(),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-rej/control/denoise/level"
				&& p == b"FAIL"),
			"le refus doit être visible sur le topic d'origine ; observations : {rows:?}"
		);
		let frames: Vec<_> = rows
			.iter()
			.filter(|(t, _, _)| {
				t.starts_with("bairelay/cam-rej/status/") && !t.ends_with("/reason")
			})
			.collect();
		assert!(
			frames.is_empty(),
			"une valeur refusée n'écrit et ne publie rien ; observations : {rows:?}"
		);
		let Some((_, payload, conserve)) = rows
			.iter()
			.find(|(t, _, _)| t == "bairelay/cam-rej/status/denoise/reason")
		else {
			panic!("un refus du pont non plus ne doit pas être silencieux ; observations : {rows:?}");
		};
		assert!(*conserve, "le motif est conservé");
		assert!(
			String::from_utf8_lossy(payload).contains("refus"),
			"le motif doit nommer le refus : {:?}",
			String::from_utf8_lossy(payload)
		);
	}

	/// `query/settings/ntp` : les trois feuilles de la famille sont publiées
	/// depuis la seule lecture, plus le JSON du bloc pour les automates.
	#[test]
	fn dispatch_query_settings_publishes_the_leaves_of_the_family() {
		on_worker(|| Box::pin(query_ntp_publishes_its_leaves()));
	}

	async fn query_ntp_publishes_its_leaves() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_NTP)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						ntp: Some(Ntp {
							version: "1.1".to_string(),
							channel_id: Some("0".to_string()),
							enable: Some("1".to_string()),
							server: Some("pool.ntp.org".to_string()),
							port: Some("123".to_string()),
							synchronize_interval: Some("1440".to_string()),
							time_zone: Some("-3600".to_string()),
						}),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let cameras = cameras_with_mock("cam-ntp", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-ntp".to_string(),
				family: Some(Family::Ntp),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		for (leaf, want) in [
			("enable", b"on".as_slice()),
			("server", b"pool.ntp.org".as_slice()),
			("interval", b"1440".as_slice()),
		] {
			let topic = format!("bairelay/cam-ntp/status/ntp/{leaf}");
			assert!(
				rows.iter()
					.any(|(t, p, r)| t == &topic && p.as_slice() == want && *r),
				"{topic} doit valoir {want:?} en conservé ; observations : {rows:?}"
			);
		}
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-ntp/status/ntp/json"
				&& p.windows(6).any(|w| w == b"server")),
			"le JSON du bloc part sur status/ntp/json ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-ntp/query/settings/ntp" && p == b"OK"),
			"la requête aboutit : OK ; observations : {rows:?}"
		);
	}

	/// Une famille que la caméra refuse voit ses états **effacés** (payload
	/// vide conservé) : Home Assistant affiche « inconnu », et non la valeur de
	/// la semaine dernière qui n'a plus rien à voir avec le matériel.
	#[test]
	fn dispatch_query_settings_refused_family_clears_states() {
		on_worker(|| Box::pin(query_ntp_refuse_clears()));
	}

	async fn query_ntp_refuse_clears() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_NTP)
			.reply_with(|req| reply_err_code(req, 404))
			.build()
			.await;
		let cameras = cameras_with_mock("cam-ntp-no", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-ntp-no".to_string(),
				family: Some(Family::Ntp),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		for leaf in ["enable", "server", "interval"] {
			let topic = format!("bairelay/cam-ntp-no/status/ntp/{leaf}");
			assert!(
				rows.iter()
					.any(|(t, p, r)| t == &topic && p.is_empty() && *r),
				"{topic} doit être effacé (payload vide conservé) ; observations : {rows:?}"
			);
		}
	}

	/// Le carillon d'une sonnette **sans** carillon appairé : 483 et 484
	/// refusent (400), 486 répond 200 sans bloc. `get_ding_dong_cfg` renvoie
	/// donc une liste `None` — « la caméra n'a pas répondu » — et la feuille
	/// `status/chime/list` est effacée plutôt que publiée à `[]` : « refusé »
	/// et « vide » doivent rester deux choses distinctes. C'est exactement ce
	/// qu'a répondu la caméra de l'utilisatrice, relue en lecture seule.
	#[test]
	fn dispatch_query_chime_without_answer_not_publishes_no_list() {
		on_worker(|| Box::pin(chime_without_answer()));
	}

	async fn chime_without_answer() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_DING_DONG_CTRL)
			.reply_with(|req| reply_err_code(req, 400))
			.expect_msg(MSG_ID_GET_DING_DONG_LIST)
			.reply_with(|req| reply_err_code(req, 400))
			// Pas de 485 / 609 : sans id de carillon, ces questions ne sont même
			// pas posées — cinq commandes radio ne sont pas offertes à une caméra
			// qui n'a rien à dire.
			.expect_msg(MSG_ID_GET_DING_DONG_CFG)
			.reply_with(|req| reply_200_empty(req))
			.build()
			.await;
		let cameras = cameras_with_mock("cam-chime", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-chime".to_string(),
				family: Some(Family::Chime),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, r)| t == "bairelay/cam-chime/status/chime/list"
				&& p.is_empty()
				&& *r),
			"refusé = état effacé, surtout pas [] ; observations : {rows:?}"
		);
		assert!(
			!rows
				.iter()
				.any(|(t, p, _)| t == "bairelay/cam-chime/status/chime/list" && p == b"[]"),
			"[] voudrait dire « appairé : aucun », ce que cette caméra n'a pas dit ; observations : {rows:?}"
		);
	}

	// ── Les trois promesses que ce lot répare ────────────────────────────

	/// Caméra simulée **avec** un éditeur de discovery attaché et des capacités
	/// connues. Sans publisher, ou sans capacités, `publish_discovery` ne publie
	/// rien (deux retours anticipés) : le test ne prouverait que la moitié du
	/// correctif — les états, jamais les entités.
	async fn cameras_with_mock_and_discovery(
		name: &str,
		mock: MockConnection,
		features: Vec<bairelay_mqtt::discovery::Feature>,
		mqtt: &SharedMqttClient,
	) -> (HashMap<String, Arc<CameraHandle>>, Arc<CameraHandle>) {
		let cam = Arc::new(BcCamera::from_mock_connection(mock).await);
		let handle = Arc::new(
			CameraHandle::new(minimal_camera_config(name), CancellationToken::new(), None)
				.with_discovery_publisher(bairelay_mqtt::DiscoveryPublisher::new(
					mqtt.clone(),
					"bairelay".to_string(),
					"homeassistant".to_string(),
					features.into_iter().collect(),
					"test".to_string(),
				)),
		);
		handle.set_capabilities_for_test(crate::capabilities::CameraCapabilities::default());
		handle.set_bc_camera_for_test(cam);
		let mut map = HashMap::new();
		map.insert(name.to_string(), Arc::clone(&handle));
		(map, handle)
	}

	/// Le volume général d'une sonnette, tel que le modèle 264 l'imprime.
	fn audio_cfg_general(volume: &str, talk: &str, visitor: &str) -> AudioCfg {
		AudioCfg {
			volume: volume.to_string(),
			..audio_cfg(talk, visitor)
		}
	}

	/// Le troisième volume : `<volume>` a son identifiant, son topic et son
	/// entité propres. Il s'écrit en lecture-modification-écriture, et c'est la
	/// **relecture** qui est publiée — ici 88, jamais la valeur demandée 77.
	#[test]
	fn dispatch_setting_volume_general_writes_then_publishes_the_reread() {
		on_worker(|| Box::pin(volume_general_writes_then_rereads()));
	}

	async fn volume_general_writes_then_rereads() {
		let sent = StdArc::new(std::sync::Mutex::new(String::new()));
		let capture = StdArc::clone(&sent);
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_AUDIO_CFG)
			.reply_with(|req| audio_reply(req, audio_cfg_general("5", "10", "7")))
			.expect_msg(MSG_ID_SET_AUDIO_CFG)
			.reply_with_xml(move |_req, xml| {
				let block = xml
					.audio_cfg
					.as_ref()
					.or(xml.audio_cfg_camel.as_ref())
					.expect("l'écriture porte un bloc audioCfg");
				*capture.lock().unwrap() = format!(
					"volume={}|talk={}|visitor={}",
					block.volume,
					block.talk_and_reply_volume.as_deref().unwrap_or("<absent>"),
					block.visitor_volume.as_deref().unwrap_or("<absent>"),
				);
				reply_200_empty(_req)
			})
			// La relecture : la caméra a gardé 88, pas la valeur demandée.
			.expect_msg(MSG_ID_GET_AUDIO_CFG)
			.reply_with(|req| audio_reply(req, audio_cfg_general("88", "10", "7")))
			.build()
			.await;
		let cameras = cameras_with_mock("cam-vol3", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Setting {
				camera: "cam-vol3".to_string(),
				setting: SettingCommand::AudioGeneralVolume("77".to_string()),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(
			sent.lock().unwrap().as_str(),
			"volume=77|talk=10|visitor=7",
			"le bloc renvoyé est la réponse de la lecture avec une seule balise touchée"
		);
		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, r)| t
				== "bairelay/cam-vol3/status/audio/general_volume"
				&& p == b"88"
				&& *r),
			"la valeur relue est publiée, conservée — pas la valeur demandée ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-vol3/status/audio/volume"
				&& p == b"10"),
			"le volume d'interphone garde sa propre feuille, il n'est pas écrasé ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t
				== "bairelay/cam-vol3/control/audio/general_volume"
				&& p == b"OK"),
			"écriture acquittée : OK ; observations : {rows:?}"
		);
		// Un refus antérieur ne doit pas survivre à une écriture réussie : la
		// famille va bien, sa raison redevient vide (et toujours conservée, pour
		// que Home Assistant efface l'ancienne phrase).
		let Some((_, cause, conserve)) =
			rows.iter().find(|(t, _, _)| t == "bairelay/cam-vol3/status/audio/reason")
		else {
			panic!("une écriture réussie efface aussi le motif ; observations : {rows:?}");
		};
		assert!(cause.is_empty(), "la famille va bien, le motif doit être vide");
		assert!(*conserve, "un topic effacé l'est par un payload vide conservé");
	}

	/// Une caméra qui n'imprime pas `<volume>` voit l'écriture refusée **sans
	/// qu'aucune trame d'écriture parte** : le pont n'invente pas au firmware une
	/// balise qu'il n'a jamais déclarée. Le motif publié le dit ; aucun état
	/// n'est publié, parce que rien n'a dst lu de nouveau.
	#[test]
	fn dispatch_setting_volume_general_refuse_without_tag() {
		on_worker(|| Box::pin(volume_general_without_tag_refuse()));
	}

	async fn volume_general_without_tag_refuse() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_AUDIO_CFG)
			.reply_with(|req| audio_reply(req, audio_cfg_general("", "10", "7")))
			// Un seul message attendu : la moindre trame de plus fermerait le
			// channel de la connexion simulée, et le FAIL ne pourrait plus venir.
			.build()
			.await;
		let cameras = cameras_with_mock("cam-vol3-absent", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Setting {
				camera: "cam-vol3-absent".to_string(),
				setting: SettingCommand::AudioGeneralVolume("77".to_string()),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t
				== "bairelay/cam-vol3-absent/control/audio/general_volume"
				&& p == b"FAIL"),
			"pas de balise, pas d'écriture : FAIL ; observations : {rows:?}"
		);
		assert!(
			!rows.iter().any(|(t, _, _)| t
				== "bairelay/cam-vol3-absent/status/audio/general_volume"),
			"on ne publie pas une valeur que la caméra n'a pas imprimée ; observations : {rows:?}"
		);
		let Some((_, cause, _)) = rows.iter().find(|(t, _, _)| {
			t == "bairelay/cam-vol3-absent/status/audio/reason"
		}) else {
			panic!("un refus ne doit pas être silencieux ; observations : {rows:?}");
		};
		assert!(
			String::from_utf8_lossy(cause).contains("refusé"),
			"le motif doit nommer le refus : {:?}",
			String::from_utf8_lossy(cause)
		);
	}

	/// Une famille jamais relue se découvre **sans aucune commande**. Ce n'est
	/// qu'après une lecture réussie que ses entités deviennent modifiables, et le
	/// basculement de plateforme vide l'ancien topic de discovery **avant** de
	/// publier le nouveau : sinon Home Assistant garde une entité fantôme qui
	/// pointe vers un interrupteur qui n'existe plus.
	#[test]
	fn discovery_ntp_without_command_until_the_first_read() {
		on_worker(|| Box::pin(discovery_ntp_becomes_writable()));
	}

	async fn discovery_ntp_becomes_writable() {
		let connexion = MockConnection::new()
			.expect_msg(MSG_ID_GET_NTP)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						ntp: Some(Ntp {
							version: "1.1".to_string(),
							channel_id: Some("0".to_string()),
							enable: Some("1".to_string()),
							server: Some("pool.ntp.org".to_string()),
							port: Some("123".to_string()),
							synchronize_interval: Some("1440".to_string()),
							time_zone: Some("-3600".to_string()),
						}),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let (cameras, cam) = cameras_with_mock_and_discovery(
			"cam-lecture",
			connexion,
			vec![bairelay_mqtt::discovery::Feature::Ntp],
			&mqtt,
		)
		.await;

		let ancien = "homeassistant/binary_sensor/bairelay_cam-lecture_ntp_enable/config";
		let nouveau = "homeassistant/switch/bairelay_cam-lecture_ntp_enable/config";

		// La discovery du démarrage, avant la moindre lecture de réglages.
		cam.publish_discovery().await.expect("première discovery");
		let rows = mock.published();
		let Some((_, payload, _)) = rows.iter().find(|(t, _, _)| t == ancien) else {
			panic!("l'interrupteur se découvre d'abord en binary_sensor ; observations : {rows:?}");
		};
		let json: serde_json::Value =
			serde_json::from_slice(payload).expect("une config est du JSON");
		assert!(
			json.get("command_topic").is_none(),
			"aucune commande ne doit être promise avant la première lecture : {json}"
		);
		assert!(!rows.iter().any(|(t, _, _)| t == nouveau), "le switch n'existe pas encore");

		// L'opératrice demande la relecture de la famille : elle a donc lieu.
		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-lecture".to_string(),
				family: Some(Family::Ntp),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		let Some(empty) = rows
			.iter()
			.position(|(t, p, r)| t == ancien && p.is_empty() && *r)
		else {
			panic!("l'ancienne config doit être vidée (payload vide conservé) ; observations : {rows:?}");
		};
		let Some(switch) = rows.iter().position(|(t, _, _)| t == nouveau) else {
			panic!("la famille relue doit devenir modifiable ; observations : {rows:?}");
		};
		assert!(
			empty < switch,
			"la rétractation doit précéder la nouvelle config : vide={empty} switch={switch}"
		);
		let (_, payload, _) = &rows[switch];
		let json: serde_json::Value =
			serde_json::from_slice(payload).expect("une config est du JSON");
		assert_eq!(
			json["command_topic"],
			"bairelay/cam-lecture/control/ntp/enable",
			"la commande attendue porte la famille et la feuille"
		);
		assert_eq!(
			json["unique_id"], "bairelay_cam-lecture_ntp_enable",
			"l'identifiant ne change pas au passage de plateforme : c'est l'entité vue par HA qui change d'nom"
		);
	}

	/// La caméra de l'utilisatrice répond `<MdAlarm>` au message 46. Ce porteur
	/// se lit mais ne se renvoie pas : les deux feuilles restent publiées (elles
	/// sont justes), le motif explique pourquoi elles ne sont pas réglables, et
	/// aucune entité ne promet de commande.
	#[test]
	fn md_alarm_stays_en_read_seule_and_the_says() {
		on_worker(|| Box::pin(md_alarm_read_seule()));
	}

	async fn md_alarm_read_seule() {
		let connexion = MockConnection::new()
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						md_alarm: Some(MdAlarm {
							version: "1.1".to_string(),
							sens_info_new: Some(SensInfoNew {
								enable: Some("1".to_string()),
								sensitivity: Some("3".to_string()),
								..Default::default()
							}),
							..Default::default()
						}),
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let (cameras, cam) = cameras_with_mock_and_discovery(
			"cam-md",
			connexion,
			vec![bairelay_mqtt::discovery::Feature::MotionSettings],
			&mqtt,
		)
		.await;

		// La discovery du démarrage, telle qu'elle a lieu en production.
		cam.publish_discovery().await.expect("première discovery");
		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-md".to_string(),
				family: Some(Family::Motion),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		for (leaf, value) in [("enable", "on"), ("sensitivity", "3")] {
			let topic = format!("bairelay/cam-md/status/motion/{leaf}");
			assert!(
				rows.iter().any(|(t, p, _)| t == &topic && p == value.as_bytes()),
				"la lecture a bien eu lieu, la feuille est publiée : {topic} ; observations : {rows:?}"
			);
		}
		let Some((_, cause, conserve)) = rows
			.iter()
			.find(|(t, _, _)| t == "bairelay/cam-md/status/motion/reason")
		else {
			panic!(
				"une famille lue mais non réécrivable doit le dire ; observations : {rows:?}"
			);
		};
		assert!(*conserve, "le motif est conservé");
		assert!(
			String::from_utf8_lossy(cause).contains("read-only"),
			"le motif doit dire la lecture seule, pas un défaut de lecture : {:?}",
			String::from_utf8_lossy(cause)
		);
		// Et la discovery reste ce qu'elle a toujours dst pour cette famille.
		assert!(
			rows.iter().any(|(t, p, r)| t
				== "homeassistant/binary_sensor/bairelay_cam-md_motion_enable/config"
				&& !p.is_empty()
				&& *r),
			"l'interrupteur de mouvement se découvre en binary_sensor ; observations : {rows:?}"
		);
		for (topic, payload, _) in rows.iter().filter(|(t, p, _)| {
			t.starts_with("homeassistant/") && t.ends_with("/config") && !p.is_empty()
		}) {
			let json: serde_json::Value =
				serde_json::from_slice(payload).expect("une config est du JSON");
			assert!(
				json.get("command_topic").is_none(),
				"{topic} promet une commande alors que <MdAlarm> ne se réécrit pas"
			);
		}
	}

	/// `<VideoInput>` tel que la sonnette l'imprime : les cinq curseurs et le
	/// jour/nuit dans le même bloc, rien d'inventé.
	fn image_xml(bright: &str) -> BcXml {
		BcXml {
			video_input: Some(VideoInput {
				version: "1.1".to_string(),
				channel_id: Some("0".to_string()),
				bright: Some(bright.to_string()),
				contrast: Some("64".to_string()),
				saturation: Some("64".to_string()),
				hue: Some("0".to_string()),
				sharpen: Some("3".to_string()),
				day_night: Some("Auto".to_string()),
				..Default::default()
			}),
			..Default::default()
		}
	}

	/// Ce qui s'est passé sur le terrain au premier déploiement : les curseurs
	/// d'image, écrits avec succès une heure plus tôt, répondent `400` à la salve
	/// complète des quinze lectures. Ce code veut dire « pas maintenant » — le dépôt
	/// le rejoue déjà ailleurs (`bc_protocol::services.rs`, cinq tentatives) — et il
	/// ne dit **rien** de ce que la caméra accepte ou refuse d'écrire. La famille
	/// garde donc ses entités modifiables, et le topic `reason` explique le refus
	/// **sans** laisser croire à une interdiction : la phrase porte d'abord la
	/// conséquence, « la famille reste publiée modifiable », parce que c'est la
	/// moitié que l'opératrice regarde et qu'elle doit survivre à la coupe à 160
	/// caractères.
	#[test]
	fn read_refused_en_400_not_locks_not_and_the_says() {
		on_worker(|| Box::pin(image_quarante_hundred_not_locks_nothing()));
	}

	async fn image_quarante_hundred_not_locks_nothing() {
		let connexion = MockConnection::new()
			// Première relecture : la caméra répond, la famille devient modifiable.
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_200_xml(req, image_xml("128")))
			// Seconde relecture : les deux tentatives échouent (la première est
			// suivie d'un rejeu unique, cf. `reglages::read_family`).
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_err_code(req, 400))
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_err_code(req, 400))
			.build()
			.await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let (cameras, cam) = cameras_with_mock_and_discovery(
			"cam-400",
			connexion,
			vec![bairelay_mqtt::discovery::Feature::Image],
			&mqtt,
		)
		.await;

		let curseur = "homeassistant/number/bairelay_cam-400_image_bright/config";
		let cause_topic = "bairelay/cam-400/status/image/reason";
		let relire = || ControlCommand::QuerySettings {
			camera: "cam-400".to_string(),
			family: Some(Family::Image),
		};

		cam.publish_discovery().await.expect("première discovery");
		let rows = mock.published();
		assert!(
			!rows.iter().any(|(t, _, _)| t == curseur),
			"jamais relue, la famille ne promet aucune commande ; observations : {rows:?}"
		);

		dispatch_control(relire(), &cameras, &mqtt, "bairelay").await;
		let rows = mock.published();
		let Some((_, payload, _)) = rows.iter().rev().find(|(t, _, _)| t == curseur) else {
			panic!("la famille relue doit promettre son curseur ; observations : {rows:?}");
		};
		let json: serde_json::Value = serde_json::from_slice(payload).expect("une config est du JSON");
		assert_eq!(
			json["command_topic"], "bairelay/cam-400/control/image/bright",
			"la lecture a rendu le curseur réglable"
		);
		assert!(
			rows.iter().any(|(t, p, r)| t == cause_topic && p.is_empty() && *r),
			"une lecture réussie vide le motif ; observations : {rows:?}"
		);
		// Marqueur : tout ce qui est publié à partir d ici suit la relecture qui
		// va échouer.
		let after = rows.len();

		dispatch_control(relire(), &cameras, &mqtt, "bairelay").await;

		let rows = mock.published();
		let burst: Vec<&(String, Vec<u8>, bool)> = rows.iter().skip(after).collect();
		for (topic, _, _) in rows.iter().skip(after) {
			assert!(
				!topic.starts_with("homeassistant/"),
				"une relecture qui rate ne doit publier aucune discovery, \
				 donc n'en rétracter et n'en rétrograder aucune : {topic} ; \
				 observations : {burst:?}"
			);
		}
		let Some((_, cause, conserve)) =
			rows.iter().rev().find(|(t, _, _)| t == cause_topic)
		else {
			panic!("un refus ne doit pas être silencieux ; observations : {rows:?}");
		};
		let mot = String::from_utf8_lossy(cause).to_string();
		assert!(mot.contains("400"), "le motif doit citer le code reçu : {mot}");
		assert!(
			mot.contains("stays published as writable"),
			"et annonce d'abord la conséquence, avant le détail technique : {mot}"
		);
		assert!(
			!mot.contains("not locked"),
			"le refused passager a sa propre phrase, celle du verdict undetermined \
			 ne doit pas resservir : {mot}"
		);
		assert!(*conserve, "un motif se publie conservé");
		assert!(
			burst.iter().any(|(t, p, _)| t == "bairelay/cam-400/status/image/bright" && p.is_empty()),
			"des valeurs qu'on n'a pas pu relire ne restent pas affichées comme vraies : {burst:?}"
		);
	}

	/// Le même refus, mais sur une famille **jamais relue** : c'est le cas qui
	/// comptait vraiment pour l'opératrice, celui du démarrage. Une caméra qui
	/// boude une lecture ne doit pas condamner la famille à l'état initial en
	/// lecture seule — `image` est précisément de celles que le firmware refuse de
	/// **lire** (400) et qu'il laisse pourtant **écrire**, écriture relue et
	/// vérifiée deux fois sur le terrain. La commande est donc publiée, et la
	/// raison du refus avec.
	#[test]
	fn quarante_hundred_on_a_family_never_read_the_publishes_writable() {
		on_worker(|| Box::pin(never_read_then_quarante_hundred()));
	}

	async fn never_read_then_quarante_hundred() {
		let connexion = MockConnection::new()
			// Les deux tentatives de `read_family` : le rejeu unique après un 400.
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_err_code(req, 400))
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_err_code(req, 400))
			.build()
			.await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let (cameras, cam) = cameras_with_mock_and_discovery(
			"cam-400-neuf",
			connexion,
			vec![bairelay_mqtt::discovery::Feature::Image],
			&mqtt,
		)
		.await;

		cam.publish_discovery().await.expect("première discovery");
		let rows = mock.published();
		assert!(
			!rows.iter().any(|(t, _, _)| t
				== "homeassistant/number/bairelay_cam-400-neuf_image_bright/config"),
			"au démarrage rien n'est promis : {rows:?}"
		);

		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-400-neuf".to_string(),
				family: Some(Family::Image),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		let curseur = "homeassistant/number/bairelay_cam-400-neuf_image_bright/config";
		let Some((_, payload, _)) = rows.iter().rev().find(|(t, _, _)| t == curseur) else {
			panic!(
				"un refused passager ne doit pas laisser la family dans son état initial \
				 de lecture seule ; observations : {rows:?}"
			);
		};
		let json: serde_json::Value = serde_json::from_slice(payload).expect("une config est du JSON");
		assert_eq!(
			json["command_topic"], "bairelay/cam-400-neuf/control/image/bright",
			"la famille est publiée modifiable malgré le refus de lecture"
		);
		let Some((_, cause, _)) =
			rows.iter().rev().find(|(t, _, _)| t == "bairelay/cam-400-neuf/status/image/reason")
		else {
			panic!("et la raison du refus s'affiche à côté ; observations : {rows:?}");
		};
		let mot = String::from_utf8_lossy(cause).to_string();
		assert!(mot.contains("400") && mot.contains("writable"), "les deux ensemble : {mot}");
	}

	/// La limite de la correction : un refus qui n'est **pas** une raison passagère
	/// ne promet toujours rien. Un `500` n'est pas un « pas maintenant » de ce
	/// dialecte, et une caméra qui répond mal à ce point n'a rien permis du tout :
	/// la famille attend une vraie lecture, et le motif reprend la mention que
	/// rien n'est verrouillé. Sans ce test, la correction serait une promesse
	/// générale d'écrire sans jamais rien avoir lu.
	#[test]
	fn cinq_hundred_not_promises_nothing_and_the_says() {
		on_worker(|| Box::pin(image_cinq_hundred_not_locks_nothing()));
	}

	async fn image_cinq_hundred_not_locks_nothing() {
		let connexion = MockConnection::new()
			// Une seule tentative : le rejeu est réservé au 400, re-tenter un `500`
			// serait payer deux fois une radio qui va mal.
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_err_code(req, 500))
			.build()
			.await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let (cameras, cam) = cameras_with_mock_and_discovery(
			"cam-500",
			connexion,
			vec![bairelay_mqtt::discovery::Feature::Image],
			&mqtt,
		)
		.await;

		cam.publish_discovery().await.expect("première discovery");
		// Marqueur : la discovery de démarrage publie les jumeaux en lecture
		// seule ; seul ce qui suit la salve compte ici.
		let after = mock.published().len();
		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-500".to_string(),
				family: Some(Family::Image),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		for (topic, payload, _) in rows.iter().skip(after) {
			assert!(
				!(topic.starts_with("homeassistant/") && !payload.is_empty()),
				"un verdict undetermined ne publie aucune discovery : {topic} ; observations : {:?}",
				&rows[after..]
			);
		}
		let Some((_, cause, _)) =
			rows.iter().rev().find(|(t, _, _)| t == "bairelay/cam-500/status/image/reason")
		else {
			panic!("le refus reste dit ; observations : {rows:?}");
		};
		let mot = String::from_utf8_lossy(cause).to_string();
		assert!(mot.contains("500"), "le motif cite le code : {mot}");
		assert!(mot.contains("not locked"), "et le verdict undetermined garde sa phrase : {mot}");
	}

	/// La suite du même incident, à l'endroit où le terrain l'a vérifié : une
	/// relecture **réussie après** un refus. Les curseurs deviennent modifiables
	/// et le motif redevient vide — c'est la moitié que la règle doit à
	/// l'opératrice, sans quoi une caméra bien portante continuerait d'afficher
	/// sa bouderie d'hier.
	#[test]
	fn reread_ok_after_a_refusal_releases_writable_and_empty_the_cause() {
		on_worker(|| Box::pin(reread_ok_after_quarante_hundred()));
	}

	async fn reread_ok_after_quarante_hundred() {
		let connexion = MockConnection::new()
			// Premier essai : « pas maintenant ».
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_err_code(req, 400))
			// Le rejeu unique, et cette fois la caméra parle.
			.expect_msg(MSG_ID_GET_IMAGE)
			.reply_with(|req| reply_200_xml(req, image_xml("128")))
			.build()
			.await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let (cameras, cam) = cameras_with_mock_and_discovery(
			"cam-rejeu",
			connexion,
			vec![bairelay_mqtt::discovery::Feature::Image],
			&mqtt,
		)
		.await;

		cam.publish_discovery().await.expect("première discovery");
		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-rejeu".to_string(),
				family: Some(Family::Image),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-rejeu/status/image/bright"
				&& p == b"128"),
			"la seconde tentative a lu, la valeur relue est publiée ; observations : {rows:?}"
		);
		let Some((_, cause, conserve)) =
			rows.iter().rev().find(|(t, _, _)| t == "bairelay/cam-rejeu/status/image/reason")
		else {
			panic!("la famille va bien, sa raison doit être écrite puis vidée ; observations : {rows:?}");
		};
		assert!(cause.is_empty(), "une relecture réussie efface le motif : {cause:?}");
		assert!(*conserve, "un topic effacé l'est par un payload vide conservé");
		assert!(
			rows.iter().any(|(t, p, _)| t
				== "homeassistant/number/bairelay_cam-rejeu_image_bright/config"
				&& !p.is_empty()),
			"et la famille devient modifiable ; observations : {rows:?}"
		);
	}

	/// L'autre espèce de refus, celle qui **verrouille** : la réponse 44
	/// n'imprime pas `<OsdChannelName>`, le porteur du nom affiché et du
	/// filigrane. Trois des quatre feuilles n'ont donc rien à renvoyer au setter
	/// 45, et nous les refusons — pas la caméra. La famille reste en lecture
	/// seule, le motif nomme la balise manquante, et l'état lu (l'horodatage) est
	/// quand même publié.
	#[test]
	fn osd_without_osdchannelname_stays_en_read_seule_and_the_says() {
		on_worker(|| Box::pin(osd_without_tag_read_seule()));
	}

	async fn osd_without_tag_read_seule() {
		let connexion = MockConnection::new()
			.expect_msg(MSG_ID_GET_OSD)
			.reply_with(|req| {
				reply_200_xml(
					req,
					BcXml {
						osd_datetime: Some(OsdDatetime {
							version: "1.1".to_string(),
							channel_id: Some("0".to_string()),
							enable: Some("1".to_string()),
							..Default::default()
						}),
						// Pas d'`<OsdChannelName>` : c'est le cas de la caméra
						// de terrain, et le seul avec `<MdAlarm>` qui verrouille.
						..Default::default()
					},
				)
			})
			.build()
			.await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();
		let (cameras, cam) = cameras_with_mock_and_discovery(
			"cam-osd",
			connexion,
			vec![bairelay_mqtt::discovery::Feature::Osd],
			&mqtt,
		)
		.await;

		cam.publish_discovery().await.expect("première discovery");
		dispatch_control(
			ControlCommand::QuerySettings {
				camera: "cam-osd".to_string(),
				family: Some(Family::Osd),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-osd/status/osd/datetime_enable"
				&& p == b"on"),
			"la lecture a bien eu lieu, la feuille lue est publiée ; observations : {rows:?}"
		);
		let Some((_, cause, _)) =
			rows.iter().rev().find(|(t, _, _)| t == "bairelay/cam-osd/status/osd/reason")
		else {
			panic!("une famille verrouillée le dit ; observations : {rows:?}");
		};
		let mot = String::from_utf8_lossy(cause).to_string();
		assert!(
			mot.contains("OsdChannelName"),
			"le motif doit nommer la balise absente : {mot}"
		);
		assert!(mot.contains("read-only"), "et le statut de la famille : {mot}");
		for (topic, payload, _) in
			rows.iter().filter(|(t, p, _)| t.starts_with("homeassistant/") && t.ends_with("/config") && !p.is_empty())
		{
			let json: serde_json::Value =
				serde_json::from_slice(payload).expect("une config est du JSON");
			assert!(
				json.get("command_topic").is_none(),
				"{topic} promet une commande alors que le porteur du nom n'existe pas"
			);
		}
	}

	// ── Intervalle PIR (212 / 213) : même porteur, autre balise ──────────

	/// Caméra simulée dont la carte d'aptitudes annonce `rfAlarm` en lecture et
	/// écriture. `has_ability_rw` lit la carte que le login remplit chez
	/// l'utilisatrice ; une connexion simulée ne la reçoit pas, donc on la
	/// déclare ici. C'est l'équivalent d'un `abilityInfo` qui annonce `rfAlarm`,
	/// pas une façon de sauter la porte.
	async fn cameras_with_rf_alarm(
		name: &str,
		mock: MockConnection,
	) -> HashMap<String, Arc<CameraHandle>> {
		let cam = Arc::new(BcCamera::from_mock_connection(mock).await);
		cam.declare_ability("rfAlarm").await;
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config(name),
			CancellationToken::new(),
			None,
		));
		handle.set_bc_camera_for_test(cam);
		let mut map = HashMap::new();
		map.insert(name.to_string(), handle);
		map
	}

	/// `<interval>` se lit et s'écrit avec les deux mêmes messages que la
	/// sensibilité (212 / 213) : la trame d'écriture doit donc rendre le bloc lu
	/// **entier**, une seule balise touchée, et c'est la relecture qui est
	/// publiée — pas la valeur demandée. Les bornes annoncées par le firmware
	/// ne sont pas des entités ; elles se lisent dans `status/pir/json`, et ce
	/// test le vérifie aussi.
	#[test]
	fn dispatch_setting_interval_pir_writes_then_publishes_the_reread() {
		on_worker(|| Box::pin(interval_pir_writes_then_rereads()));
	}

	async fn interval_pir_writes_then_rereads() {
		let sent = StdArc::new(std::sync::Mutex::new(String::new()));
		let capture = StdArc::clone(&sent);
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_PIR_ALARM)
			.reply_with(|req| answer_pir(req, pir_cfg_with_interval(30, 120)))
			.expect_msg(MSG_ID_START_PIR_ALARM)
			.reply_with_xml(move |_req, xml| {
				let bloc = xml
					.rf_alarm_cfg
					.as_ref()
					.expect("l'écriture porte un bloc RfAlarmCfg");
				*capture.lock().unwrap() = format!(
					"enable={}|sensi={:?}|interval={:?}|min={:?}|max={:?}",
					bloc.enable,
					bloc.sensiValue,
					bloc.interval,
					bloc.intervalSecMin,
					bloc.intervalSecMax,
				);
				reply_200_empty(_req)
			})
			// La relecture : la caméra a ramené l'intervalle à 60 s et annonce
			// maintenant 60 comme maximum.
			.expect_msg(MSG_ID_GET_PIR_ALARM)
			.reply_with(|req| answer_pir(req, pir_cfg_with_interval(60, 60)))
			.build()
			.await;
		let cameras = cameras_with_rf_alarm("cam-pir-int", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Setting {
				camera: "cam-pir-int".to_string(),
				setting: SettingCommand::PirInterval("45".to_string()),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		assert_eq!(
			sent.lock().unwrap().as_str(),
			"enable=1|sensi=Some(21)|interval=Some(45)|min=Some(5)|max=Some(120)",
			"le bloc renvoyé est celui qui a dst lu, avec <interval> seul touché"
		);
		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, r)| t == "bairelay/cam-pir-int/status/pir/interval"
				&& p == b"60"
				&& *r),
			"c'est l'intervalle relu qui est publié, conservé ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-pir-int/status/pir/sensitivity"
				&& p == b"21"),
			"la sensibilité garde sa propre feuille, elle n'est pas écrasée ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-pir-int/control/pir/interval"
				&& p == b"OK"),
			"écriture acquittée : OK ; observations : {rows:?}"
		);
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-pir-int/status/pir/json"
				&& String::from_utf8_lossy(p).contains("\"intervalSecMax\":60")),
			"les bornes annoncées par le firmware sont lisibles dans le JSON relu ; observations : {rows:?}"
		);
		let Some((_, cause, conserve)) =
			rows.iter().find(|(t, _, _)| t == "bairelay/cam-pir-int/status/pir/reason")
		else {
			panic!("une écriture réussie efface aussi le motif ; observations : {rows:?}");
		};
		assert!(cause.is_empty(), "la famille va bien, le motif doit être vide");
		assert!(*conserve, "un topic effacé l'est par un payload vide conservé");
	}

	/// Une caméra dont le `<RfAlarmCfg>` n'imprime pas `<interval>` voit
	/// l'écriture refusée **sans qu'aucune trame 213 parte** : le pont n'invente
	/// pas au firmware une balise qu'il n'a jamais déclarée.
	#[test]
	fn dispatch_setting_interval_pir_refuse_without_tag() {
		on_worker(|| Box::pin(interval_pir_without_tag_refuse()));
	}

	async fn interval_pir_without_tag_refuse() {
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_GET_PIR_ALARM)
			.reply_with(|req| {
				// Orthographe de l'ancien firmware : `sensitivity`, et aucun
				// `<interval>` — le réglage n'existe pas ici.
				answer_pir(
					req,
					RfAlarmCfg {
						version: "1.0".to_string(),
						enable: 1,
						sensitivity: Some(4),
						..Default::default()
					},
				)
			})
			// Un seul message attendu : la moindre trame de plus fermerait le
			// channel simulé, et le FAIL ne pourrait plus venir.
			.build()
			.await;
		let cameras = cameras_with_rf_alarm("cam-pir-int-absent", mock).await;
		let (mqtt, mock) = bairelay_mqtt::test_support::mock_client();

		dispatch_control(
			ControlCommand::Setting {
				camera: "cam-pir-int-absent".to_string(),
				setting: SettingCommand::PirInterval("45".to_string()),
			},
			&cameras,
			&mqtt,
			"bairelay",
		)
		.await;

		let rows = mock.published();
		assert!(
			rows.iter().any(|(t, p, _)| t == "bairelay/cam-pir-int-absent/control/pir/interval"
				&& p == b"FAIL"),
			"pas de balise, pas d'écriture : FAIL ; observations : {rows:?}"
		);
		assert!(
			!rows
				.iter()
				.any(|(t, _, _)| t == "bairelay/cam-pir-int-absent/status/pir/interval"),
			"on ne publie pas un intervalle que la caméra n'a pas imprimée ; observations : {rows:?}"
		);
		let Some((_, cause, _)) = rows
			.iter()
			.find(|(t, _, _)| t == "bairelay/cam-pir-int-absent/status/pir/reason")
		else {
			panic!("un refus ne doit pas être silencieux ; observations : {rows:?}");
		};
		let mot = String::from_utf8_lossy(cause).to_string();
		assert!(mot.contains("refusé"), "le motif doit nommer le refus : {mot}");
		assert!(mot.contains("interval"), "et la balise en cause : {mot}");
	}
}
