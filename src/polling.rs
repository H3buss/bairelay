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
const POLL_STEP: Duration = Duration::from_millis(250);

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
	in_flight: Arc<AtomicBool>,
	/// Instant de la dernière interrogation **qui a obtenu une session**. Une
	/// tentative manquée n'y écrit pas : elle n'a rien coûté, elle ne doit
	/// donc rien interdire.
	last_paid: Mutex<Option<Instant>>,
}

impl Guard {
	pub(crate) fn new() -> Self {
		Self {
			in_flight: Arc::new(AtomicBool::new(false)),
			last_paid: Mutex::new(None),
		}
	}

	/// Réserver le droit de partir : faux si une interrogation tourne déjà ou
	/// si la dernière **payée** est trop récente. La réservation se prend ici,
	/// en synchrone, parce que deux annonces à la milliseconde près ne doivent
	/// pas acheter deux sessions.
	///
	/// Un refus ne consomme rien : ni la date, ni le drapeau.
	fn reserve(&self, interval: Duration, now: Instant) -> bool {
		if self.in_flight.swap(true, Ordering::SeqCst) {
			return false;
		}
		let last = self
			.last_paid
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		if let Some(at) = *last {
			if now - at < interval {
				// Le drapeau n'était pas pris : on le rend tel qu'il était.
				drop(last);
				self.in_flight.store(false, Ordering::SeqCst);
				return false;
			}
		}
		true
	}

	/// La session est là : cette interrogation a payé une session, elle a donc
	/// droit à son intervalle avant la suivante.
	fn mark_paid(&self, at: Instant) {
		*self.last_paid
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(at);
	}

	/// La tâche est terminée : elle rend le drapeau. Appelé par le garde‑fou
	/// `Drop` de la tâche, donc aussi quand elle est annulée en plein `await`.
	fn release(&self) {
		self.in_flight.store(false, Ordering::SeqCst);
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
pub(crate) fn try_once(
	guard: &Arc<Guard>,
	handle: Arc<CameraHandle>,
	hold: Duration,
	interval: Duration,
	cancel: &CancellationToken,
) -> bool {
	if !guard.reserve(interval, Instant::now()) {
		tracing::debug!("poll: no start cam={} (one is running, or the last session is younger than {} s)",
			handle.name(),
			interval.as_secs());
		return false;
	}
	let cancel = cancel.clone();
	tokio::spawn(run_task(Arc::clone(guard), handle, hold, cancel));
	true
}

/// Le corps de l'interrogation, une fois la garde réservée.
async fn run_task(guard: Arc<Guard>, handle: Arc<CameraHandle>, hold: Duration, cancel: CancellationToken) {
	/// Le drapeau se rend quoi qu'il arrive : une tâche annulée au milieu d'un
	/// `await` ne doit pas verrouiller la caméra pour toujours.
	struct Release(Arc<Guard>);
	impl Drop for Release {
		fn drop(&mut self) {
			self.0.release();
		}
	}
	let _release = Release(Arc::clone(&guard));

	let cam = handle.name().to_owned();
	let start = tokio::time::Instant::now();

	// Le geste entier : tenir le verrou. C'est lui qui fait la session, et
	// `idle_disconnect` rend la caméra dès qu'on le lâche.
	//
	// Le carnet porte le compte de verrous et pas seulement le nôtre : un verrou
	// pris alors qu'il en existe deja un veut dire qu'une session est ouverte ou
	// en train de se monter ailleurs. C'est exactement ce qu'on veut voir quand
	// une séance dure quatre minutes au lieu de trente secondes.
	let wake_guard = handle.wake_lock().acquire();
	tracing::debug!("poll: wake lock taken cam={cam} locks={}",
		handle.wake_lock().count());

	let ceiling = start + hold + HOLD_AFTER_SESSION;
	let mut deadline = start + hold;
	let mut seen = None;
	loop {
		let now = tokio::time::Instant::now();
		if now >= deadline {
			break;
		}
		if seen.is_none() && handle.bc_camera_concrete().is_some() {
			let at = now;
			seen = Some(at);
			guard.mark_paid(at);
			// La session vient d'arriver : lui laisser le temps de l'abonnement.
			deadline = (at + HOLD_AFTER_SESSION).min(ceiling);
			tracing::debug!("poll: session established after {} ms cam={} — the alarm listener holds the lane, lock kept for {} more s",
				elapsed_ms(start, at),
				cam,
				(deadline - at).as_secs());
		}
		let remaining = deadline - tokio::time::Instant::now();
		tokio::select! {
			_ = cancel.cancelled() => break,
			_ = handle.cancel_token().cancelled() => break,
			_ = tokio::time::sleep(remaining.min(POLL_STEP)) => {}
		}
	}

	// Le verrou rendu avant la ligne de carnet : le compte qui suit est celui
	// qui reste, donc celui qui dit si la caméra va vraiment se rendormir.
	drop(wake_guard);
	let locks_left = handle.wake_lock().count();
	match seen {
		Some(at) => tracing::debug!("poll: lock released after {} ms cam={} (session seen at {} ms) locks={}",
			elapsed_ms(start, tokio::time::Instant::now()),
			cam,
			elapsed_ms(start, at),
			locks_left),
		None => tracing::debug!("poll: no session after {} ms cam={} locks={} — nothing polled, the slow path keeps the lane"
			, elapsed_ms(start, tokio::time::Instant::now()), cam, locks_left),
	}
}

/// Millisecondes écoulées entre deux instants, pour les lignes du carnet.
fn elapsed_ms(start: Instant, now: Instant) -> u128 {
	now.saturating_duration_since(start).as_millis()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_first_request_always_passes() {
		let guard = Guard::new();
		assert!(guard.reserve(Duration::from_secs(30), Instant::now()));
	}

	#[test]
	fn only_one_poll_at_a_time() {
		// La seconde demande tombe pendant que la première tient le verrou :
		// refusée, sans même regarder l'intervalle.
		let guard = Guard::new();
		let now = Instant::now();
		assert!(guard.reserve(Duration::from_secs(30), now));
		assert!(
			!guard.reserve(Duration::from_secs(0), now),
			"a poll in flight blocks the next one, zero interval or not"
		);
	}

	#[test]
	fn the_interval_only_punishes_a_won_session() {
		// Le coeur de la politique : une tentative qui n'a pas eu de session
		// n'écrit rien, la prochaine annonce de réveil repart donc.
		let guard = Guard::new();
		let now = Instant::now();
		assert!(guard.reserve(Duration::from_secs(30), now));
		guard.release();
		assert!(
			guard.reserve(Duration::from_secs(30), now + Duration::from_secs(2)),
			"nothing was paid for, so nothing is forbidden"
		);
		guard.release();
		// Là, la session est venue : l'intervalle s'applique.
		guard.mark_paid(now + Duration::from_secs(4));
		assert!(!guard.reserve(Duration::from_secs(30), now + Duration::from_secs(10)));
		assert!(guard.reserve(Duration::from_secs(30), now + Duration::from_secs(35)));
	}

	#[test]
	fn an_interval_refusal_costs_nothing() {
		// Un refus ne doit pas repousser la date de la prochaine tentative
		// autorisée : sinon une annonce de bruit en ferait rater une bonne.
		let guard = Guard::new();
		let now = Instant::now();
		guard.mark_paid(now);
		assert!(!guard.reserve(Duration::from_secs(30), now + Duration::from_secs(1)));
		assert!(guard.reserve(Duration::from_secs(30), now + Duration::from_secs(31)));
	}
}
