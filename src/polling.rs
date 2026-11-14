//! Interroger la caméra dans la seconde qui suit son annonce de réveil.
//!
//! Pourquoi ce module existe. L'annonce UDP que la caméra pousse toute seule
//! dit deux choses, mais pas au même moment : `event` 3 prévient qu'elle se
//! réveille — cent millisecondes après le bouton — et `event` 4 avec `reason`
//! 19 ne confirme l'appui que **dix‑huit secondes plus tard**, à la fin de
//! l'enregistrement ([`crate::annonces`] publie ce second moment). Dix‑huit
//! secondes pour une sonnette, c'est trop.
//!
//! Ce que fait l'interrogation, et seulement cela : à `event` 3, prendre le
//! verrou de réveil de la caméra et le tenir. C'est ce verrou qui dit à la
//! boucle de connexion d'aller joindre la caméra — le même geste que la salve
//! de lecture ([`crate::burst`]). Une fois la session établie, l'écouteur
//! d'alarmes du pont ([`crate::camera_tasks::motion_listener`]) s'abonne de
//! lui‑même au message 31 et lit l'état visiteur de chaque poussée : si la
//! caméra tient cet état, l'appui part par le chemin déjà écrit, dédupliqué.
//!
//! Ce que ce module ne fait pas, et c'est le contrat :
//!
//! - **il n'ouvre pas son propre abonnement.** Une souscription aux alarmes
//!   tient déjà le canal de poussée de la session ; en ouvrir une seconde sur
//!   le même message serait inventer un protocole que la caméra n'a pas écrit.
//!   On laisse donc l'écouteur en place faire la lecture et la publication.
//! - **il ne publie rien lui‑même** : ni mouvement, ni appui. Un réveil n'est
//!   pas un mouvement, et l'appui appartient à [`crate::camera_tasks`].
//! - **il ne part pas deux fois.** Une seule interrogation à la fois par
//!   caméra, et un intervalle minimum entre deux *payées* : en mode nominal,
//!   la détection de mouvement réveille la caméra sur chaque véhicule.
//!
//! Le verrou, pas la session. Ce module ne peut pas promettre une session : il
//! peut seulement tenir la porte ouverte et attendre. Les quatre chronos relevés
//! le 13 novembre 2026 le disent mieux qu'un réglage théorique — 604 ms,
//! 2 413 ms, 604 ms, et une fois **rien du tout au bout de 20 s**, la caméra
//! répondant à la sollicitation douze secondes après qu'on eut renoncé. D'où la
//! politique : tenir une fenêtre bornée, regarder la session arriver sans en
//! dépendre, et **ne pas consommer l'intervalle quand rien n'a été obtenu** —
//! une tentative qui n'a rien payé ne doit pas interdire la suivante, qui sera
//! peut‑être la bonne.
//!
//! Coût, énoncé pour être mesuré et non oublié : une interrogation qui réussit
//! achète une session TCP, et une session allonge l'activité de la caméra —
//! 249 s relevées contre 19 s sans nous. Chaque étape est écrite dans le carnet
//! du pont ([`bairelay_wake_server::consigner`]), horodatée à la milliseconde,
//! parce que le journal du conteneur n'est pas consultable à distance.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::camera::CameraHandle;

/// Rythme de la sonde qui regarde si la session est là.
const SONDE: Duration = Duration::from_millis(250);

/// Temps pendant lequel le verrou reste tenu **après** que la session est
/// apparue : c'est celui de l'abonnement aux alarmes et de la première poussée.
/// Une session qui arrive tard ne doit pas être lâchée dans la seconde.
const HOLD_AFTER_SESSION: Duration = Duration::from_secs(8);

/// Une interrogation à la fois, par caméra.
///
/// La garde vit dans la tâche qui consomme les annonces, une par caméra
/// configurée — jamais globale : deux caméras n'ont pas à se punir l'une
/// l'autre.
pub(crate) struct Guard {
	/// Une tâche tient déjà le verrou de cette caméra. Partagée avec la tâche
	/// lancée, qui la rend en finissant — y compris si elle est annulée.
	en_cours: Arc<AtomicBool>,
	/// Instant de la dernière interrogation **qui a obtenu une session**. Une
	/// tentative manquée n'y écrit pas : elle n'a rien coûté, elle ne doit
	/// donc rien interdire.
	derniere_payee: Mutex<Option<Instant>>,
}

impl Guard {
	pub(crate) fn nouvelle() -> Self {
		Self {
			en_cours: Arc::new(AtomicBool::new(false)),
			derniere_payee: Mutex::new(None),
		}
	}

	/// Réserver le droit de partir : faux si une interrogation tourne déjà ou
	/// si la dernière **payée** est trop récente. La réservation se prend ici,
	/// en synchrone, parce que deux annonces à la milliseconde près ne doivent
	/// pas acheter deux sessions.
	///
	/// Un refus ne consomme rien : ni la date, ni le drapeau.
	fn reserver(&self, intervalle: Duration, maintenant: Instant) -> bool {
		if self.en_cours.swap(true, Ordering::SeqCst) {
			return false;
		}
		let derniere = self
			.derniere_payee
			.lock()
			.unwrap_or_else(|empoisonnee| empoisonnee.into_inner());
		if let Some(quand) = *derniere {
			if maintenant - quand < intervalle {
				// Le drapeau n'était pas pris : on le rend tel qu'il était.
				drop(derniere);
				self.en_cours.store(false, Ordering::SeqCst);
				return false;
			}
		}
		true
	}

	/// La session est là : cette interrogation a payé une session, elle a donc
	/// droit à son intervalle avant la suivante.
	fn marquer_payee(&self, quand: Instant) {
		*self.derniere_payee
			.lock()
			.unwrap_or_else(|empoisonnee| empoisonnee.into_inner()) = Some(quand);
	}

	/// La tâche est terminée : elle rend le drapeau. Appelé par le garde‑fou
	/// `Drop` de la tâche, donc aussi quand elle est annulée en plein `await`.
	fn rendre(&self) {
		self.en_cours.store(false, Ordering::SeqCst);
	}
}

/// Lancer une interrogation si la garde l'accepte. Vrai quand la tâche est
/// partie — le carnet du pont porte alors les lignes qui disent quand la
/// session est venue, ou qu'elle n'est pas venue.
///
/// `tenue` borne la fenêtre d'attente : le verrou est rendu au plus tard à ce
/// moment‑là, sauf si la session arrive si tard que [`HOLD_AFTER_SESSION`]
/// serait écrasé — auquel cas la fenêtre est allongée d'autant, sans dépasser
/// `tenue + HOLD_AFTER_SESSION`.
pub(crate) fn tenter(
	garde: &Arc<Guard>,
	handle: Arc<CameraHandle>,
	tenue: Duration,
	intervalle: Duration,
	cancel: &CancellationToken,
) -> bool {
	if !garde.reserver(intervalle, Instant::now()) {
		tracing::debug!("interrogation : pas de depart camera={} (une tourne, ou la derniere session a moins de {} s)",
			handle.name(),
			intervalle.as_secs());
		return false;
	}
	let cancel = cancel.clone();
	tokio::spawn(tache(Arc::clone(garde), handle, tenue, cancel));
	true
}

/// Le corps de l'interrogation, une fois la garde réservée.
async fn tache(garde: Arc<Guard>, handle: Arc<CameraHandle>, tenue: Duration, cancel: CancellationToken) {
	/// Le drapeau se rend quoi qu'il arrive : une tâche annulée au milieu d'un
	/// `await` ne doit pas verrouiller la caméra pour toujours.
	struct Release(Arc<Guard>);
	impl Drop for Release {
		fn drop(&mut self) {
			self.0.rendre();
		}
	}
	let _rendu = Release(Arc::clone(&garde));

	let nom = handle.name().to_owned();
	let debut = tokio::time::Instant::now();

	// Le geste entier : tenir le verrou. C'est lui qui fait la session, et
	// `idle_disconnect` rend la caméra dès qu'on le lâche.
	//
	// Le carnet porte le compte de verrous et pas seulement le nôtre : un verrou
	// pris alors qu'il en existe deja un veut dire qu'une session est ouverte ou
	// en train de se monter ailleurs. C'est exactement ce qu'on veut voir quand
	// une séance dure quatre minutes au lieu de trente secondes.
	let verrou = handle.wake_lock().acquire();
	tracing::debug!("interrogation : verrou de reveil pris camera={nom} verrous={}",
		handle.wake_lock().count());

	let plafond = debut + tenue + HOLD_AFTER_SESSION;
	let mut echeance = debut + tenue;
	let mut vue = None;
	loop {
		let maintenant = tokio::time::Instant::now();
		if maintenant >= echeance {
			break;
		}
		if vue.is_none() && handle.bc_camera_concrete().is_some() {
			let quand = maintenant;
			vue = Some(quand);
			garde.marquer_payee(quand);
			// La session vient d'arriver : lui laisser le temps de l'abonnement.
			echeance = (quand + HOLD_AFTER_SESSION).min(plafond);
			tracing::debug!("interrogation : session etablie apres {} ms camera={} — l'ecouteur d'alarmes a la main, verrou garde encore {} s",
				duree_ms(debut, quand),
				nom,
				(echeance - quand).as_secs());
		}
		let reste = echeance - tokio::time::Instant::now();
		tokio::select! {
			_ = cancel.cancelled() => break,
			_ = handle.cancel_token().cancelled() => break,
			_ = tokio::time::sleep(reste.min(SONDE)) => {}
		}
	}

	// Le verrou rendu avant la ligne de carnet : le compte qui suit est celui
	// qui reste, donc celui qui dit si la caméra va vraiment se rendormir.
	drop(verrou);
	let verrous_restants = handle.wake_lock().count();
	match vue {
		Some(quand) => tracing::debug!("interrogation : verrou rendu apres {} ms camera={} (session vue a {} ms) verrous={}",
			duree_ms(debut, tokio::time::Instant::now()),
			nom,
			duree_ms(debut, quand),
			verrous_restants),
		None => tracing::debug!("interrogation : aucune session apres {} ms camera={} verrous={} — rien interroge, la voie lente garde la main"
			, duree_ms(debut, tokio::time::Instant::now()), nom, verrous_restants),
	}
}

/// Millisecondes écoulées entre deux instants, pour les lignes du carnet.
fn duree_ms(debut: Instant, maintenant: Instant) -> u128 {
	maintenant.saturating_duration_since(debut).as_millis()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn la_premiere_demande_passe_toujours() {
		let garde = Guard::nouvelle();
		assert!(garde.reserver(Duration::from_secs(30), Instant::now()));
	}

	#[test]
	fn une_seule_interrogation_a_la_fois() {
		// La seconde demande tombe pendant que la première tient le verrou :
		// refusée, sans même regarder l'intervalle.
		let garde = Guard::nouvelle();
		let maintenant = Instant::now();
		assert!(garde.reserver(Duration::from_secs(30), maintenant));
		assert!(
			!garde.reserver(Duration::from_secs(0), maintenant),
			"une interrogation en cours bloque la suivante, intervalle nul ou non"
		);
	}

	#[test]
	fn l_intervalle_ne_punit_qu_une_session_obtenue() {
		// Le coeur de la politique : une tentative qui n'a pas eu de session
		// n'écrit rien, la prochaine annonce de réveil repart donc.
		let garde = Guard::nouvelle();
		let maintenant = Instant::now();
		assert!(garde.reserver(Duration::from_secs(30), maintenant));
		garde.rendre();
		assert!(
			garde.reserver(Duration::from_secs(30), maintenant + Duration::from_secs(2)),
			"rien n'a ete paye : rien n'est interdit"
		);
		garde.rendre();
		// Là, la session est venue : l'intervalle s'applique.
		garde.marquer_payee(maintenant + Duration::from_secs(4));
		assert!(!garde.reserver(Duration::from_secs(30), maintenant + Duration::from_secs(10)));
		assert!(garde.reserver(Duration::from_secs(30), maintenant + Duration::from_secs(35)));
	}

	#[test]
	fn un_refus_sur_intervalle_ne_consomme_rien() {
		// Un refus ne doit pas repousser la date de la prochaine tentative
		// autorisée : sinon une annonce de bruit en ferait rater une bonne.
		let garde = Guard::nouvelle();
		let maintenant = Instant::now();
		garde.marquer_payee(maintenant);
		assert!(!garde.reserver(Duration::from_secs(30), maintenant + Duration::from_secs(1)));
		assert!(garde.reserver(Duration::from_secs(30), maintenant + Duration::from_secs(31)));
	}
}
