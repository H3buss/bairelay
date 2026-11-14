//! Salve de lecture déclenchée par le **wake spontané** de la caméra.
//!
//! # Pourquoi ce module existe
//!
//! La batterie est l'urgence du terrain : plus aucun minuteur ne lit la caméra
//! (`idle_disconnect = true`, `battery_update` réglé sur « jamais »), donc le
//! niveau de batterie ne remontait plus que si une personne demandait
//! `query/settings`. Or la caméra, elle, se wakele toute seule : appui sur le
//! bouton, remontée de mouvement, enregistrement poussé sur le channel du faux
//! cloud. Chaque wake est une fenêtre où l'on peut lire **gratuitement** — la
//! radio est déjà ouverte.
//!
//! Une salve, c'est cette fenêtre : quelques commandes, payées au wake, jamais
//! ailleurs.
//!
//! # Les quatre règles, et pourquoi elles sont verrouillées par des tests
//!
//! 1. **Un wake = une salve.** La garde ([`BurstGuard`]) refuse une seconde
//!    salve tant que la première tourne *et* pendant [`GUARD_WINDOW`] après
//!    elle. Trois sources peuvent annoncer le même wake (la session, le
//!    webhook, la poussée) : sans cela, on paierait trois salves d'un coup.
//! 2. **Jamais périodique, jamais au démarrage.** Ce module n'a aucune
//!    `interval`, aucune `sleep` de cadencement, et n'est appelé d'aucun
//!    path de démarrage : la mémoire de
//!    [`crate::family_state::families_prouvees`] suffit à publier la discovery
//!    sans radio. La seule `sleep` du module est la sonde d'attente de session
//!    ([`await_session`]), bornée, qui n'émet aucune commande.
//! 3. **Jamais pendant un flux.** [`CameraHandle::flux_en_cours`] refuse la
//!    salve tant qu'un `StreamSource` a des abonnés : une caméra qui envoie de
//!    la vidéo n'est pas le moment de lui demander dix blocs XML. Le dialogue
//!    audio, lui, est un **processus séparé** (`bairelay parler`,
//!    `pont-audio`) : le pont ne peut pas le voir, et le rapport le dit — ce
//!    garde-fou-là n'est pas dans ce binaire.
//! 4. **Une borne haute de commandes, journalisée.** [`COMMAND_BUDGET`] est la
//!    dépense maximale d'une salve, évaluée avant de partir ([`build_plan`])
//!    et écrite dans le journal avec ce qui a dst sauté. Une famille chère
//!    (`chime`, cinq commandes) ne mange pas toute la fenêtre sans que cela se
//!    voie.
//!
//! # Ce qui se passe quand la caméra refuse
//!
//! Deux refus, et ils ne se ressemblent pas.
//!
//! * **La caméra n'a rien répondu** ([`Verdict::Unknown`] : pas de session,
//!   injoignable) : la salve s'arrête **sur place**, les familles déjà lues
//!   restent publiées, et le verrou est rendu — insister sur une caméra qui ne
//!   répond pas ne se paie pas deux fois.
//! * **La caméra a répondu « non »** ([`Verdict::Transient`] : un `400`, un
//!   délai, un bloc inexploitable) : la salve **continue**. Mesuré le 13/11 sur
//!   cette sonnette : `image` (26), `osd` (44), `motion` (46) et `mask` (52)
//!   rendent `400` y compris caméra **bien éveillée**, alors que batterie (253),
//!   PIR (212) et carillon répondent dans la même session. Le refus de l'une
//!   n'apprend donc rien sur la suivante, et l'ancienne règle — s'arrêter au
//!   premier `400` — a privé le carillon de sa lecture pendant des semaines :
//!   la tournée morte à quatre commandes sur huit, `chime` jamais atteint.
//!
//! Deux garde-fous empêchent le « continue » de tourner au gaspillage : deux
//! refus **consécutifs** rendent la session ([`MAX_CONSECUTIVE_REFUSALS`]) — une caméra
//! qui dit non deux fois de suite ne dira probablement rien de mieux — et une
//! famille refusée [`FAMILY_MAX_REFUSALS`] wakes de suite sort du plan
//! ([`BurstGuard::family_saturated`]) : on ne rachète pas cent fois la même
//! porte fermée. Le motif de chaque refus reste publié par
//! [`crate::settings::read_family`] lui-même.
//!
//! # La mémoire des familles
//!
//! La salve ne dévine aucune famille : elle relit celles dont la modifiabilité a
//! **déjà dst prouvée** ([`crate::family_state`]), et écrit cette mémoire
//! **une seule fois** par salve — un seul appel à
//! [`CameraHandle::mark_families_read`] en fin de salve, qui est le seul
//! path qui touche le disque.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bairelay_mqtt::settings::Family;
use bairelay_mqtt::{SharedMqttClient, StatusPublisher};
use bairelay_neolink_core::bc_protocol::BcCamera;
use tokio::task::JoinHandle;

use crate::camera::CameraHandle;
use crate::settings::{self, read_family, Verdict};

/// Fenêtre pendant laquelle un second wake ne relance **pas** de salve.
///
/// Quelques secondes, pas davantage : le terrain montre le bouton, la poussée et
/// l'alarme webhooks dans la même demi-seconde. Trop court, on paie deux salves
/// pour un doigt ; trop long, une seconde visite trente secondes plus tard ne
/// lit rien. Vingt secondes couvre le hold de mouvement typique.
pub(crate) const GUARD_WINDOW: Duration = Duration::from_secs(20);

/// Borne haute du nombre de commandes radio qu'une salve peut payer.
///
/// C'est **la** ligne à bouger si l'opératrice trouve la salve trop avare ou
/// trop coûteuse : elle est écrite dans le journal de chaque salve, et les
/// familles qu'elle a fait sauter sont comptées avec elle.
pub(crate) const COMMAND_BUDGET: usize = 8;

/// plafond de durée d'une salve, au-delà duquel on rend la session.
///
/// Une caméra qui dort mal répond une fois sur dix et le pont ne doit pas
/// tenir le wake ouvert pour ça : au-delà, ce qui est lu est publié, le reste
/// attendra le prochain wake.
const TIME_BUDGET: Duration = Duration::from_secs(25);

/// Combien de temps attendre que la session Baïchuan soit établie.
///
/// Le wake tient le verrou, la boucle de connexion de la caméra fait le
/// reste : vingt secondes couvrent un établissement lent sur Wi-Fi sans
/// immobiliser la caméra si elle est morte.
const SESSION_WAIT: Duration = Duration::from_secs(20);

/// Période de la sonde d'attente de session.
const SESSION_POLL: Duration = Duration::from_millis(200);

/// Après autant de wakes consécutifs sans réponse à la trame 253, la salve
/// arrête de la payer.
///
/// Le pendant, dans la salve, du verrouillage `battery_unsupported` du poller —
/// que la salve ne peut pas attendre : avec `battery_update` réglé sur
/// « jamais », le poller ne tick plus et ne verrouille donc jamais rien. Une
/// caméra secteur ne mérite pas une commande perdue à chaque appui, indéfiniment.
const BATTERY_MAX_REFUSALS: usize = 3;

/// Après autant de wakes consécutifs à refuser une famille, la salve cesse de
/// la payer — jusqu'au prochain redémarrage du pont.
///
/// Trois, et non un : le terrain montre des `400` qui s'effacent d'un wake à
/// l'autre (`image` a déjà rendu son bloc après un refus). Trois ratés d'affilée
/// en revanche, sur une caméra qu'on sait en forme puisque la batterie et le PIR
/// répondent, est une porte fermée : chaque nouvelle tentative est une seconde
/// de radio et rien d'autre.
const FAMILY_MAX_REFUSALS: usize = 3;

/// Autant de refus **d'affilée dans la même tournée** rendent la session.
///
/// Le « continue » acquis le 13/11 — un `400` sur une famille ne dit rien de la
/// suivante — ne doit pas devenir une tournée de quinze portes fermées. Deux est
/// le compromis : assez pour laisser à `chime` sa chance derrière `image`, assez
/// peu pour ne pas brûler un wake contre un firmware muet.
const MAX_CONSECUTIVE_REFUSALS: usize = 2;

/// Ce qui a wakelé la caméra. Publié avec l'état de la salve : « est-ce que
/// l'appui du bouton me coûte une salve ? » est la première question de
/// l'opératrice qui soupçonne la batterie de partir vite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
	/// Appui sur le bouton de la sonnette.
	Press,
	/// Remontée de mouvement (alarme de session, alarme relayée, PIR webhook).
	Motion,
	/// Trame entrante sur le channel du faux cloud (poussée TCP).
	Push,
}

impl Wake {
	/// Le mot court publié dans l'état de salve.
	pub(crate) fn slug(self) -> &'static str {
		match self {
			Wake::Press => "press",
			Wake::Motion => "motion",
			Wake::Push => "push",
		}
	}
}

/// Pourquoi la salve s'est arrêtée — et donc ce que l'opératrice lira sur
/// `status/<cam>/status/burst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
	/// Pas de MQTT : rien à publier, donc rien à lire.
	NoMqtt,
	/// Un flux est en cours : la règle 3.
	Stream,
	/// Aucune session Baïchuan n'a dst établie pendant [`SESSION_WAIT`].
	NoSession,
	/// [`MAX_CONSECUTIVE_REFUSALS`] refus d'affilée, ou une tentative impossible
	/// ([`Verdict::Unknown`]) : la salve s'arrête là, motifs publiés.
	Refused,
	/// La borne [`COMMAND_BUDGET`] a fait sauter au moins une famille.
	Budget,
	/// [`TIME_BUDGET`] dépassée.
	Duration,
	/// Le pont s'arrête (jeton de la caméra annulé).
	Cancelled,
	/// Tout le plan est passé.
	Done,
}

impl Stop {
	/// Le mot court publié dans l'état de salve.
	pub(crate) fn slug(self) -> &'static str {
		match self {
			Stop::NoMqtt => "no-mqtt",
			Stop::Stream => "stream",
			Stop::NoSession => "no-session",
			Stop::Refused => "refused",
			Stop::Budget => "budget",
			Stop::Duration => "duration",
			Stop::Cancelled => "cancelled",
			Stop::Done => "done",
		}
	}
}

/// Ce qu'une caméra sait déjà faire, entre deux salves.
///
/// Un seul exemplaire par caméra, posée sur le [`CameraHandle`] : c'est l'objet
/// qui répond à « ai-je déjà payé une salve pour ce wake ? », question qui se
/// pose simultanément depuis la session, le webhook et la poussée.
#[derive(Debug)]
pub(crate) struct BurstGuard {
	/// Une salve tourne-t-elle en ce moment ?
	in_flight: AtomicBool,
	/// Instant de départ de la dernière salve acceptée.
	last: Mutex<Option<Instant>>,
	/// Fenêtre de refus après une salve ([`GUARD_WINDOW`] en production).
	window: Duration,
	/// Refus consécutifs de la trame 253, vus par les salves.
	batterie_refus: AtomicUsize,
	/// Refus consécutifs par famille, vus par les salves de **cette** caméra.
	///
	/// En mémoire seulement, et c'est voulu : la mémoire du lot
	/// ([`crate::family_state`]) dit si une famille est *écrivable*, question
	/// durable ; ici la question est « est-ce que je continue à payer une
	/// lecture que cette caméra refuse depuis trois wakes ? », question de
	/// séance. Un redémarrage rouvre donc les trois tentatives — c'est exactement
	/// ce qu'un redémarrage veut dire pour l'opératrice.
	refus_familles: Mutex<HashMap<Family, usize>>,
}

impl BurstGuard {
	/// Garde neuve, avec la fenêtre de refus à appliquer.
	pub(crate) fn new(window: Duration) -> Self {
		BurstGuard {
			in_flight: AtomicBool::new(false),
			last: Mutex::new(None),
			window,
			batterie_refus: AtomicUsize::new(0),
			refus_familles: Mutex::new(HashMap::new()),
		}
	}

	/// Réserver la salve : `true` si l'appelant peut la lancer.
	///
	/// Le `maintenant` est un paramètre parce que la fenêtre se vérifie à deux
	/// instants près et qu'un test ne peut attendre vingt secondes. Le refus
	/// rend la garde disponible (`en_cours` remis à `false`) : c'est l'appelant
	/// qui n'a pas le droit de tourner, pas la caméra.
	pub(crate) fn prendre(&self, now: Instant) -> bool {
		if self.in_flight.swap(true, Ordering::SeqCst) {
			return false;
		}
		let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
		if let Some(since) = *last {
			if now.duration_since(since) < self.window {
				drop(last);
				self.in_flight.store(false, Ordering::SeqCst);
				return false;
			}
		}
		*last = Some(now);
		drop(last);
		true
	}

	/// Une salve vient de se terminer : la garde redevient disponible, mais la
	/// fenêtre, elle, court toujours.
	pub(crate) fn liberer(&self) {
		self.in_flight.store(false, Ordering::SeqCst);
	}

	/// Une salve tourne-t-elle ? Observé par les tests : la production décide
	/// avec [`BurstGuard::prendre`], qui réserve et répond dans le même élan.
	#[cfg(test)]
	pub(crate) fn in_flight(&self) -> bool {
		self.in_flight.load(Ordering::SeqCst)
	}

	/// Faut-il cesser de payer la trame 253 ?
	pub(crate) fn batterie_epuisee(&self) -> bool {
		self.batterie_refus.load(Ordering::SeqCst) >= BATTERY_MAX_REFUSALS
	}

	/// Compter le résultat d'une trame 253 : un succès remet le compteur à zéro.
	pub(crate) fn note_battery(&self, repondu: bool) {
		if repondu {
			self.batterie_refus.store(0, Ordering::SeqCst);
		} else {
			self.batterie_refus.fetch_add(1, Ordering::SeqCst);
		}
	}

	/// Une famille vient d'être refusée : compter, et dire combien de fois de
	/// suite. La lecture qui aboutit appelle [`BurstGuard::note_family_read`]
	/// et remet le compteur à zéro — une famille qui revient n'est pas une
	/// famille morte.
	pub(crate) fn note_family_refused(&self, family: Family) -> usize {
		let mut refused = self.refus_familles.lock().unwrap_or_else(|p| p.into_inner());
		let compteur = refused.entry(family).or_insert(0);
		*compteur += 1;
		*compteur
	}

	/// Une famille vient de répondre : elle n'est plus en dette. La clé reste
	/// en place à `0` (une clé par famille, quinze au plus) plutôt que de
	/// déséquilibrer la table à chaque salve.
	pub(crate) fn note_family_read(&self, family: Family) {
		let mut refused = self.refus_familles.lock().unwrap_or_else(|p| p.into_inner());
		refused.insert(family, 0);
	}

	/// Faut-il cesser de payer cette famille ? [`FAMILY_MAX_REFUSALS`] refus de
	/// suite, et plus rien jusqu'au prochain succès — qui, par définition, ne
	/// peut plus venir tant que la famille est écartée : c'est le redémarrage du
	/// pont qui rejoue la chance, et c'est assumé.
	pub(crate) fn family_saturated(&self, family: Family) -> bool {
		self.refus_familles
			.lock()
			.unwrap_or_else(|p| p.into_inner())
			.get(&family)
			.is_some_and(|n| *n >= FAMILY_MAX_REFUSALS)
	}
}

/// Rend la garde quand la tâche de salve se termine, y compris en paniquant.
///
/// Sans cela, une panique dans une lecture de réglage condamnerait toutes les
/// salves suivantes de cette caméra — le pire silence qui soit sur une sonnette
/// dont on surveille justement la batterie.
struct BurstRelease(Arc<BurstGuard>);

impl Drop for BurstRelease {
	fn drop(&mut self) {
		self.0.liberer();
	}
}

/// Le plan d'une salve : ce qui sera lu, ce que ça coûte, ce qui a dst sauté.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BurstPlan {
	/// Les familles qui seront relues, dans l'ordre de lecture.
	pub(crate) families: Vec<Family>,
	/// Total des commandes évalué ([`cout`]).
	pub(crate) commands: usize,
	/// Familles écartées parce que la borne ne les payait pas.
	pub(crate) skipped: usize,
}

/// Combien de commandes radio une famille coûte à relire.
///
/// Les nombres viennent de [`crate::settings::sync_family`], branche par
/// branche — pas d'une estimation : `chime` est la seule famille à cinq commandes
/// (483, 484, 485, 486, 609), et `ai` n'en coûte aucune parce qu'aucune balise
/// d'activation n'a jamais dst documentée.
pub(crate) fn cost(family: Family) -> usize {
	match family {
		Family::Battery => 1,
		Family::Chime => 5,
		Family::Ai => 0,
		_ => 1,
	}
}

/// Le rang de lecture dans la fenêtre, mesuré le 13/11.
///
/// La borne de [`COMMAND_BUDGET`] n'est pas la contrainte réelle. Ce qui coupe
/// la tournée, c'est la caméra : `status/burst` du 13/11 rend `etat:"refus"` à
/// quatre commandes sur huit, à la fois pour `raison:"appui"` et pour
/// `raison:"mouvement"`, et la ligne `event 4` de la même minute chiffre la
/// session à une quinzaine de secondes. Trier seulement par coût laissait donc
/// partir les familles dans l'ordre du catalogue et rendre la session avant
/// celles qui servent.
///
/// Trois rangs, chacun adossé à une mesure :
///
/// * **premiers, `motion` et `osd`** — les deux familles dont l'état sert chaque
///   jour, et les deux qui répondent `200` dans la forme de trame que ce
///   micrologiciel accepte (campagne de sonde du même jour) ;
/// * **ensuite, tout le reste**, du moins cher au plus cher, comme avant ;
/// * **en dernier, `image`** — le créneau 26 rend un `<Shelter>` et non le
///   `<VideoInput>` demandé, la lecture se solde par une erreur de nom qui
///   efface les valeurs déjà publiées, et la commande coûte exactement ce que
///   coûte une lecture utile. La famille n'est pas supprimée : elle garde son
///   verdict
///   et son motif, elle passe simplement après celles qui rapportent quelque
///   chose quand la fenêtre se referme.
fn rank(family: Family) -> u8 {
	match family {
		Family::Motion | Family::Osd => 0,
		Family::Image => 2,
		_ => 1,
	}
}

/// Établir le plan : le rang mesuré d'abord, puis la famille la moins chère,
/// jusqu'à la borne, et sauter le reste sans s'arrêter là.
///
/// Trois règles, et c'est le test qui les dit :
///
/// * **sauter et continuer**, et non s'arrêter à la première famille trop
///   chère — une famille à cinq commandes ne doit pas faire perdre les six
///   lectures à une commande qui tiennent dans la même fenêtre ;
/// * **le rang avant le coût** ([`rang`]) : quand la caméra rend la session au
///   quatrième ordre, l'ordre d'entrée est ce qui détermine ce qui est revenu ;
/// * **payer du moins cher au plus cher** à rang égal — à budget égal, ce qui
///   compte est le nombre de familles revenues vérifiées pour un wake de la
///   caméra. Le tri est **stable** : à rang et coût égaux l'ordre reçu est
///   conservé, et deux salves sur le même ensemble de familles se ressemblent.
///
/// C'est aussi ce qui rend la borne **haute** au sens strict :
/// [`BurstPlan::commandes`] ne dépasse jamais `budget`.
pub(crate) fn build_plan(families: &[Family], budget: usize) -> BurstPlan {
	let mut paid: Vec<(u8, usize, Family)> =
		families.iter().copied().map(|f| (rank(f), cost(f), f)).collect();
	paid.sort_by_key(|(rank, cost, _)| (*rank, *cost));
	let mut plan = BurstPlan { families: Vec::new(), commands: 0, skipped: 0 };
	for (_, cost, family) in paid {
		if plan.commands + cost > budget {
			plan.skipped += 1;
			continue;
		}
		plan.commands += cost;
		plan.families.push(family);
	}
	plan
}

/// Les familles que la mémoire dit prouvées, moins la batterie.
///
/// La batterie est lue à part, en premier : c'est pour elle que la salve existe,
/// et son niveau se publie sur un topic que `sync_family` ne remplit pas.
pub(crate) fn families_to_read(camera: &str) -> Vec<Family> {
	crate::family_state::families_prouvees(camera)
		.into_iter()
		.filter(|f| *f != Family::Battery)
		.collect()
}

/// Déclencher une salve si le wake a droit à la sienne.
///
/// Appelé **après** la publication de l'événement qui l'a déclenché — jamais
/// avant : un appui doit être publié avant la première commande de lecture, sinon
/// l'automatisation de l'opératrice découvre l'appui avec le retard de toute la
/// salve. La décision de partir est **ici**, synchrone, parce que deux wakes à
/// la milliseconde près doivent donner une seule salve.
///
/// Rend la poignée de la tâche pour les tests ; les appelants de production
/// l'ignorent : la salve est détachée par construction.
pub(crate) fn trigger(handle: Arc<CameraHandle>, reason: Wake) -> Option<JoinHandle<()>> {
	if !handle.burst().prendre(Instant::now()) {
		tracing::debug!(
			camera = %handle.name(),
			reason = reason.slug(),
			"wake sans salve : une salve tourne déjà, ou ce wake tombe dans la fenêtre du précédent"
		);
		return None;
	}
	tracing::info!(
		camera = %handle.name(),
		reason = reason.slug(),
		budget = COMMAND_BUDGET,
		"salve de wake engagée"
	);
	Some(tokio::spawn(task(handle, reason)))
}

/// Le corps de la salve, une fois la garde réservée.
async fn task(handle: Arc<CameraHandle>, reason: Wake) {
	let _fin = BurstRelease(Arc::clone(handle.burst()));

	let Some(mqtt) = handle.mqtt_client() else {
		let stop = Stop::NoMqtt;
		tracing::debug!(
			camera = %handle.name(),
			reason = reason.slug(),
			state = stop.slug(),
			"salve sans MQTT : rien à publier, donc rien à lire"
		);
		return;
	};
	let mqtt = SharedMqttClient::clone(mqtt);
	let prefix = handle.topic_prefix().to_string();
	let publisher = StatusPublisher::new(&mqtt, &prefix, handle.name());

	// Règle 3 : jamais pendant un flux.
	if handle.flux_in_flight() {
		tracing::info!(
			camera = %handle.name(),
			reason = reason.slug(),
			"salve refusée : un flux est en cours"
		);
		publier(&mqtt, &prefix, handle.name(), reason, Stop::Stream, 0, 0).await;
		return;
	}

	// Tenir le wake : sans ce verrou, `idle_disconnect` rendrait la caméra
	// pendant que nous lisons, et la salve perdrait sa propre session.
	let _lock = handle.wake_lock().acquire();

	let Some(bc) = await_session(&handle).await else {
		tracing::info!(
			camera = %handle.name(),
			reason = reason.slug(),
			salle = ?SESSION_WAIT,
			"salve sans session Baïchuan : la caméra ne s'est pas laissée joindre"
		);
		publier(&mqtt, &prefix, handle.name(), reason, Stop::NoSession, 0, 0).await;
		return;
	};

	// Les familles que cette caméra refuse depuis [`FAMILY_MAX_REFUSALS`] wakes
	// ne repartent pas au budget : la borne de [`COMMAND_BUDGET`] est trop
	// précieuse pour une porte déjà fermée trois fois.
	let guard = handle.burst();
	let possible = families_to_read(handle.name());
	let a_lire: Vec<Family> = possible
		.iter()
		.copied()
		.filter(|f| !guard.family_saturated(*f))
		.collect();
	let ecartees = possible.len() - a_lire.len();
	let plan = build_plan(&a_lire, COMMAND_BUDGET);
	tracing::info!(
		camera = %handle.name(),
		reason = reason.slug(),
		families = plan.families.len(),
		skipped = plan.skipped,
		ecartees,
		commands = plan.commands,
		budget = COMMAND_BUDGET,
		"plan de salve"
	);

	let mut read: Vec<(Family, Verdict)> = Vec::new();
	let mut commands = 0usize;
	let tournee = tokio::time::timeout(
		TIME_BUDGET,
		burst(&handle, &bc, &publisher, &plan, &mut read, &mut commands),
	)
	.await;
	let mut stop = match tournee {
		Ok(stop) => stop,
		Err(_) => Stop::Duration,
	};
	if stop == Stop::Done && plan.skipped > 0 {
		stop = Stop::Budget;
	}

	// La seule écriture de la mémoire du lot — un appel, une écriture.
	handle.mark_families_read(&read).await;

	tracing::info!(
		camera = %handle.name(),
		reason = reason.slug(),
		state = stop.slug(),
		commands,
		budget = COMMAND_BUDGET,
		families = read.len(),
		writable = read.iter().filter(|(_, v)| *v == Verdict::Writable).count(),
		passageres = read.iter().filter(|(_, v)| *v == Verdict::Transient).count(),
		"salve de wake terminée : session rendue"
	);
	publier(&mqtt, &prefix, handle.name(), reason, stop, commands, read.len()).await;
	// `_verrou` et `_fin` se libèrent ici : la caméra peut se rendormir.
}

/// La tournée de lecture elle-même : la batterie, puis les familles du plan.
///
/// S'arrête au premier refus, borne atteinte, ou annulation. Le compteur de
/// commandes est un paramètre et non une valeur de retour, comme pour
/// [`crate::settings::sync_all`] : quand la tournée est emportée par
/// [`TIME_BUDGET`], ce qui a déjà dst payé doit rester visible dans le journal.
async fn burst(
	handle: &CameraHandle,
	bc: &BcCamera,
	publ: &StatusPublisher<'_>,
	plan: &BurstPlan,
	read: &mut Vec<(Family, Verdict)>,
	commands: &mut usize,
) -> Stop {
	let guard = handle.burst();

	// La batterie d'abord : c'est la trame 253, celle qui ne wakele pas la
	// caméra, et la raison pour laquelle la salve existe.
	if guard.batterie_epuisee() {
		tracing::debug!(
			camera = %handle.name(),
			refused = BATTERY_MAX_REFUSALS,
			"salve sans trame 253 : la batterie n'a pas répondu depuis assez de wakes pour arrêter de la payer"
		);
	} else {
		let issu = bc.battery_info().await;
		if let Ok(infos) = &issu {
			let niveau = infos.battery_percent.min(100) as u8;
			if let Err(e) = publ.publish_battery_level(niveau).await {
				tracing::debug!(camera = %handle.name(), error = %e, "publication du niveau de batterie ratée");
			}
			handle.status_cache().set_battery_level(niveau);
		}
		guard.note_battery(issu.is_ok());
		let (family, verdict) = settings::battery_read(publ, &issu).await;
		read.push((family, verdict));
		*commands += 1;
		// Un `400` sur la trame 253 ne doit pas éteindre la tournée non plus :
		// c'est exactement le cas mesuré où la batterie refuse et où le carillon,
		// juste derrière, aurait répondu. Le « stopper » de la trame 253 a déjà
		// son propre garde-fou ([`BATTERY_MAX_REFUSALS`], compté par
		// [`BurstGuard::note_battery`]).
		if verdict == Verdict::Unknown {
			tracing::info!(
				camera = %handle.name(),
				family = family.slug(),
				"salve arrêtée sur la batterie : rien n'a répondu, la session est rendue"
			);
			return Stop::Refused;
		}
	}

	// Refus **d'affilée dans cette tournée** : remis à zéro dès qu'une famille
	// répond. C'est ce compteur qui rend la session, pas celui par famille.
	let mut refus_tournee = 0usize;
	for &family in &plan.families {
		let cost = cost(family);
		if *commands + cost > COMMAND_BUDGET {
			tracing::info!(
				camera = %handle.name(),
				family = family.slug(),
				commands = *commands,
				budget = COMMAND_BUDGET,
				"salve arrêtée par la borne de commands"
			);
			return Stop::Budget;
		}
		*commands += cost;
		if handle.cancel_token().is_cancelled() {
			return Stop::Cancelled;
		}
		let (family, verdict) = read_family(bc, publ, family).await;
		read.push((family, verdict));
		match verdict {
			// La caméra a dit non : elle est donc vivante et elle répond. Ce
			// refus ne dit **rien** de la famille suivante, et l'historique de
			// cette sonnette est là pour le prouver — la tournée s'arrêtait à
			// quatre commandes et le carillon n'était plus jamais lu. On note,
			// on continue, et on ne dépasse pas [`MAX_CONSECUTIVE_REFUSALS`].
			Verdict::Transient => {
				// Deux compteurs, et pas un : `refus` est celui de **cette
				// famille** à travers les wakes (il décide si on continue de la
				// payer), `tournee` est celui de la tournée en cours (il décide si
				// on rend la session). Les confondre éteindrait la tournée dès la
				// première famille, ou la laisserait courir quinze portes fermées.
				let refused = guard.note_family_refused(family);
				refus_tournee += 1;
				tracing::info!(
					camera = %handle.name(),
					family = family.slug(),
					refused,
					plafond = FAMILY_MAX_REFUSALS,
					tournee = refus_tournee,
					"famille refusée : la salve continue, la suivante n'est pas condamnée"
				);
				if refus_tournee >= MAX_CONSECUTIVE_REFUSALS {
					tracing::info!(
						camera = %handle.name(),
						family = family.slug(),
						plafond = MAX_CONSECUTIVE_REFUSALS,
						"salve rendue après deux refus d'affilée : les families lues restent publiées"
					);
					return Stop::Refused;
				}
			}
			// Rien n'a répondu : ce n'est pas un refus, c'est l'absence. Tenir
			// la session ne peut plus rien rapporter.
			Verdict::Unknown => {
				tracing::info!(
					camera = %handle.name(),
					family = family.slug(),
					"salve arrêtée sur une tentative impossible : les families déjà lues restent publiées, la session est rendue"
				);
				return Stop::Refused;
			}
			// La famille a répondu : elle n'est plus en dette de refus, et la
			// série de refus de la tournée est rompue.
			other => {
				guard.note_family_read(family);
				refus_tournee = 0;
				let _ = other;
			}
		}
	}

	Stop::Done
}

/// Attendre que la session Baïchuan de cette caméra soit là.
///
/// Aucune commande n'est émise ici : c'est la boucle de connexion de la caméra
/// qui établit la session, tenue par le verrou que la salve (et le hold du
/// wake) détient. La sonde est bornée par [`SESSION_WAIT`] et coupée par
/// l'arrêt du pont.
///
/// `pub(crate)` parce que [`crate::polling`] attend la même session pour
/// la même raison : un verrou pris sans session obtenue ne sert à rien.
pub(crate) async fn await_session(handle: &CameraHandle) -> Option<Arc<BcCamera>> {
	let deadline = tokio::time::Instant::now() + SESSION_WAIT;
	loop {
		if let Some(bc) = handle.bc_camera_concrete() {
			return Some(bc);
		}
		if handle.cancel_token().is_cancelled() {
			return None;
		}
		let now = tokio::time::Instant::now();
		if now >= deadline {
			return None;
		}
		let remaining = deadline - now;
		tokio::select! {
			_ = handle.cancel_token().cancelled() => return None,
			_ = tokio::time::sleep(remaining.min(SESSION_POLL)) => {}
		}
	}
}

/// Publier l'état de la salve : `status/<cam>/status/burst`, conservé.
///
/// Le sujet n'a pas d'entité Home Assistant discovery — volontairement : c'est
/// un topic de diagnostic, et la discovery publierait une entité que
/// l'opératrice n'a pas demandée. Le motif détaillé, famille par famille, est
/// déjà publié sur `status/<famille>/reason` par `reglages`.
async fn publier(
	mqtt: &SharedMqttClient,
	prefix: &str,
	camera: &str,
	reason: Wake,
	state: Stop,
	commands: usize,
	families: usize,
) {
	let topic = format!("{prefix}/{camera}/status/burst");
	let payload = format!(
		r#"{{"reason":"{reason}","state":"{state}","commands":{commands},"budget":{budget},"families":{families}}}"#,
		reason = reason.slug(),
		state = state.slug(),
		commands = commands,
		budget = COMMAND_BUDGET,
		families = families,
	);
	if let Err(e) = mqtt.publish_retained(&topic, payload.as_bytes()).await {
		tracing::debug!(camera = %camera, error = %e, "publication de l'état de salve ratée");
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::test_helpers::minimal_camera_config;
	use bairelay_mqtt::test_support::mock_client;
	use bairelay_neolink_core::bc::model::{
		BcXml, MSG_ID_BATTERY_INFO, MSG_ID_GET_MOTION_DETECT, MSG_ID_GET_PRIVACY_MASK,
	};
	use bairelay_neolink_core::bc_protocol::connection::mock::{
		reply_200_xml, reply_err_code, MockConnection,
	};
	use bairelay_neolink_core::bc_protocol::BcCamera;
	use std::sync::{Arc, Mutex};
	use tokio_util::sync::CancellationToken;

	/// Une sonnette dont la batterie répond 63 %.
	fn batterie(answer: u32) -> BcXml {
		BcXml {
			battery_info: Some(bairelay_neolink_core::bc::xml::BatteryInfo {
				battery_percent: answer,
				charge_status: "none".to_string(),
				..Default::default()
			}),
			..Default::default()
		}
	}

	/// Le même harnais que `mqtt_dispatch::tests::on_worker` : une `BcXml`
	/// complète fait plusieurs kilo-octets et chaque commande caméra en brasse au
	/// moins deux copies, donc le corps tourne sur un fil à pile explicite.
	type Body = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

	fn on_worker(body: fn() -> Body) {
		let pile = if cfg!(debug_assertions) {
			8 * 1024 * 1024
		} else {
			2 * 1024 * 1024
		};
		std::thread::Builder::new()
			.stack_size(pile)
			.spawn(move || {
				let rt = tokio::runtime::Builder::new_current_thread()
					.enable_all()
					.start_paused(true)
					.build()
					.expect("runtime du fil de test");
				rt.block_on(body());
			})
			.expect("fil de test à pile explicite")
			.join()
			.expect("la salve ne doit pas paniquer");
	}

	/// Règle 1, et le cœur du lot : deux wakes, une seule salve.
	#[test]
	fn a_wake_triggers_a_seule_burst() {
		on_worker(|| Box::pin(a_seule_burst()));
	}

	async fn a_seule_burst() {
		let trace = Arc::new(Mutex::new(Vec::new()));
		let trace_cmd = Arc::clone(&trace);
		// Deux commandes scriptées de plus que nécessaire : si une seconde salve
		// partait, elle les paierait et la trace s'allongerait.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(move |req| {
				trace_cmd.lock().unwrap().push(req.meta.msg_id);
				reply_200_xml(req, batterie(63))
			})
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(|req| reply_200_xml(req, batterie(63)))
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(|req| reply_200_xml(req, batterie(63)))
			.build()
			.await;
		let (mqtt, capture) = mock_client();
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config("cam-salve"),
			CancellationToken::new(),
			Some(mqtt),
		));
		handle.set_bc_camera_for_test(Arc::new(BcCamera::from_mock_connection(mock).await));

		let first = trigger(Arc::clone(&handle), Wake::Press).expect("la première salve part");
		// Le second wake du même wake : l'alarme webhook qui suit l'appui.
		let second = trigger(Arc::clone(&handle), Wake::Motion);
		assert!(second.is_none(), "un second wake dans la fenêtre ne paie pas de salve");
		first.await.expect("la salve n'a pas paniqué");

		assert_eq!(
			*trace.lock().unwrap(),
			vec![MSG_ID_BATTERY_INFO],
			"une seule salve = une seule trame 253, et rien d'autre"
		);
		let state = capture
			.published_payloads()
			.into_iter()
			.find(|(t, _)| t == "bairelay/cam-salve/status/burst")
			.expect("l'état de salve est publié");
		let state = String::from_utf8_lossy(&state.1).into_owned();
		assert!(state.starts_with("{\"reason\":\"press\""), "{state}");
		assert!(state.contains("\"commands\":1"), "{state}");
		assert!(!state.contains("\"state\":\"refus\""), "{state}");
	}

	/// Pas de wake = pas une commande. C'est la propridst que la batterie
	/// réclame : sans elle, le pont est juste un sondeur de plus.
	#[test]
	fn no_wake_not_sends_no_command() {
		on_worker(|| Box::pin(no_wake()));
	}

	async fn no_wake() {
		let trace = Arc::new(Mutex::new(Vec::new()));
		let trace_cmd = Arc::clone(&trace);
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(move |req| {
				trace_cmd.lock().unwrap().push(req.meta.msg_id);
				reply_200_xml(req, batterie(63))
			})
			.build()
			.await;
		let (mqtt, _capture) = mock_client();
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config("cam-repos"),
			CancellationToken::new(),
			Some(mqtt),
		));
		handle.set_bc_camera_for_test(Arc::new(BcCamera::from_mock_connection(mock).await));

		// Le temps de laisser partir ne serait-ce qu'une tâche : rien ne part.
		tokio::time::sleep(Duration::from_millis(50)).await;
		assert!(trace.lock().unwrap().is_empty(), "aucun wake, aucune commande caméra");
		assert_eq!(handle.wake_lock().count(), 0, "sans salve, rien ne tient le wake");
	}

	/// Le `press` doit être **publié** avant la première commande de la salve,
	/// sinon l'automatisation de l'opératrice découvre l'appui avec le retard de
	/// toute la lecture.
	#[test]
	fn the_press_part_before_the_first_command() {
		on_worker(|| Box::pin(press_of_abord()));
	}

	async fn press_of_abord() {
		let vus_a_la_commande: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
		let (mqtt, capture) = mock_client();
		let capture_a_la_commande = capture.clone();
		let trace_cmd = Arc::clone(&vus_a_la_commande);
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(move |req| {
				// L'instant exact où la première commande de salve part : le
				// press doit déjà être sur le bus.
				trace_cmd
					.lock()
					.unwrap()
					.extend(capture_a_la_commande.published_topics());
				reply_200_xml(req, batterie(63))
			})
			.build()
			.await;
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config("cam-appui"),
			CancellationToken::new(),
			Some(mqtt),
		));
		handle.set_bc_camera_for_test(Arc::new(BcCamera::from_mock_connection(mock).await));

		// Le path de production : le rapport d'appui publie le press, **puis**
		// déclare le wake.
		let publisher = StatusPublisher::new(
			handle.mqtt_client().expect("ce harnais a un client MQTT"),
			handle.topic_prefix(),
			handle.name(),
		);
		crate::camera_tasks::report_doorbell_press(
			"cam-appui",
			&publisher,
			&handle.status_cache(),
			Some(&handle),
		)
		.await;
		// La salve est détachée par construction : on la laisse finir. Le temps
		// est virtuel (`start_paused`), cette attente ne coûte rien en réel.
		tokio::time::sleep(Duration::from_secs(2)).await;

		let topics = capture.published_topics();
		let position_press = topics
			.iter()
			.position(|t| t == "bairelay/cam-appui/status/doorbell")
			.expect("l'appui est publié sur le topic de sonnerie");
		let position_state = topics
			.iter()
			.position(|t| t == "bairelay/cam-appui/status/burst")
			.expect("l'état de salve est publié en fin de salve");
		assert!(
			position_press < position_state,
			"le press avant la fin de la salve ; topics : {topics:?}"
		);
		let vus = vus_a_la_commande.lock().unwrap();
		assert!(
			vus.iter().any(|t| t == "bairelay/cam-appui/status/doorbell"),
			"à la première commande de salve, le press était déjà publié ; observé : {vus:?}"
		);
	}

	/// Une famille qui refuse ne condamne pas la tournée ; deux refus d'affilée
	/// rendent la session, et les motifs sont publiés.
	#[test]
	fn burst_interrupted_publishes_and_releases_the_session() {
		on_worker(|| Box::pin(burst_interrupted()));
	}

	async fn burst_interrupted() {
		// La batterie répond, puis **deux** familles sont refusées d'un `400` —
		// le code que ce dialecte lit « pas maintenant », pas « pas ça ».
		// `read_family` en tente deux par famille : **les quatre sont
		// scriptées**, sans quoi la mock ne répond plus rien et `raw_get`,
		// qui n'a aucun délai à lui et ne doit pas en avoir, attendrait ici
		// jusqu'à la fin du test.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(|req| reply_200_xml(req, batterie(20)))
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(|req| reply_err_code(req, 400))
			.expect_msg(MSG_ID_GET_MOTION_DETECT)
			.reply_with(|req| reply_err_code(req, 400))
			.expect_msg(MSG_ID_GET_PRIVACY_MASK)
			.reply_with(|req| reply_err_code(req, 400))
			.expect_msg(MSG_ID_GET_PRIVACY_MASK)
			.reply_with(|req| reply_err_code(req, 400))
			.build()
			.await;
		let (mqtt, capture) = mock_client();
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config("cam-maussade"),
			CancellationToken::new(),
			Some(mqtt),
		));
		handle.set_bc_camera_for_test(Arc::new(BcCamera::from_mock_connection(mock).await));

		let plan = BurstPlan {
			families: vec![Family::Motion, Family::Mask],
			commands: 2,
			skipped: 0,
		};
		let (stop, commands) = {
			let publisher = StatusPublisher::new(
				handle.mqtt_client().expect("harnais avec MQTT"),
				handle.topic_prefix(),
				handle.name(),
			);
			let bc = handle.bc_camera_concrete().expect("session scriptée");
			let mut read = Vec::new();
			let mut paid = 0usize;
			let fin = burst(&handle, &bc, &publisher, &plan, &mut read, &mut paid).await;
			// Les deux familles doivent être passées : la première refusée
			// n'a pas éteint la seconde. C'est la correction du 13/11, mesurée
			// sur la vraie sonnette — `image` en `400` y tue le carillon.
			assert!(read
				.iter()
				.any(|(f, v)| *f == Family::Motion && *v == Verdict::Transient));
			assert!(
				read.iter()
					.any(|(f, v)| *f == Family::Mask && *v == Verdict::Transient),
				"la famille qui suit un `400` est tentée quand même ; observé : {read:?}"
			);
			(fin, paid)
		};
		assert_eq!(
			stop,
			Stop::Refused,
			"deux refus d'affilée rendent la session ([MAX_CONSECUTIVE_REFUSALS])"
		);
		assert_eq!(
			commands, 3,
			"trame 253 (1) + les deux families refusées (1 + 1) : les deux lectures refusées ont bien dst payées, donc tentées"
		);
		let topics = capture.published_topics();
		assert!(
			topics.iter().any(|t| t.ends_with("status/motion/reason")),
			"le motif de la première famille refusée est publié ; observé : {topics:?}"
		);
		assert!(
			topics.iter().any(|t| t.ends_with("status/mask/reason")),
			"le motif de la seconde aussi, puisque sa lecture a dst tentée ; observé : {topics:?}"
		);

		// Et la session rendue. `trigger` est le seul path qui prend le
		// verrou de wake — c'est donc lui qu'on teste pour la restitution,
		// avec une session neuve : le script ci-dessus est épuisé, et une
		// mock épuisée ne répond plus, ce qui n'apprendrait rien.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(|req| reply_200_xml(req, batterie(35)))
			.build()
			.await;
		handle.set_bc_camera_for_test(Arc::new(BcCamera::from_mock_connection(mock).await));
		let task = trigger(Arc::clone(&handle), Wake::Push).expect("salve autorisée");
		task.await.expect("pas de panique");
		assert_eq!(
			handle.wake_lock().count(),
			0,
			"une salve terminée, y compris interrompue, a rendu le verrou de wake"
		);
		assert!(!handle.burst().in_flight(), "la garde est libérée");
		assert!(
			capture.published_topics().iter().any(|t| t.ends_with("status/burst")),
			"la salve détachée a publié son état : {:?}",
			capture.published_topics()
		);
	}

	/// La garde refuse la seconde salve dans la fenêtre et l'autorise après.
	#[test]
	fn the_guard_holds_the_window() {
		let guard = BurstGuard::new(Duration::from_millis(500));
		let start = Instant::now();
		assert!(guard.prendre(start));
		assert!(!guard.prendre(start + Duration::from_millis(10)), "la fenêtre court");
		guard.liberer();
		assert!(
			!guard.prendre(start + Duration::from_millis(60)),
			"une salve terminée dans la fenêtre ne relance rien"
		);
		assert!(guard.prendre(start + Duration::from_millis(600)), "fenêtre passée");
	}

	/// Une famille que la caméra refuse depuis [`FAMILY_MAX_REFUSALS`] wakes ne
	/// repart plus au budget ; une famille qui répond efface sa dette.
	///
	/// C'est la moitié qui protège la batterie : sans elle, « continuer après un
	/// `400` » signifierait racheter quatre portes fermées à chaque appui, tous
	/// les jours, sur une sonnette sur pile.
	#[test]
	fn a_family_refused_trois_fois_sort_of_plan() {
		let guard = BurstGuard::new(Duration::from_millis(500));
		assert!(!guard.family_saturated(Family::Image), "neuve, elle est dans le plan");
		assert_eq!(guard.note_family_refused(Family::Image), 1);
		assert_eq!(guard.note_family_refused(Family::Image), 2);
		assert!(!guard.family_saturated(Family::Image), "deux refus, elle y est encore");
		assert_eq!(guard.note_family_refused(Family::Image), FAMILY_MAX_REFUSALS);
		assert!(guard.family_saturated(Family::Image), "trois refus d'affilée, stop");

		// Les autres familles n'y sont pour rien : `image` refuse et `chime`
		// se présente quand même au wake suivant.
		assert!(!guard.family_saturated(Family::Chime), "une famille n'entraîne pas les autres");

		// Et un succès rouvre la porte : la dette est effacée.
		guard.note_family_read(Family::Image);
		assert!(!guard.family_saturated(Family::Image), "une famille qui a répondu repart à zéro");

		// Le plan ne la paie plus : à budget compté, `image` (1) et `chime` (5)
		// sur une borne de 6, moins `image` écartée, laisse les 5 de `chime`.
		let guard = BurstGuard::new(Duration::from_millis(500));
		for _ in 0..FAMILY_MAX_REFUSALS {
			guard.note_family_refused(Family::Image);
		}
		let possibles = [Family::Image, Family::Chime];
		let a_lire: Vec<Family> = possibles
			.iter()
			.copied()
			.filter(|f| !guard.family_saturated(*f))
			.collect();
		assert_eq!(a_lire, vec![Family::Chime], "la famille saturée ne repart pas au budget");
		assert_eq!(build_plan(&a_lire, 6).families, vec![Family::Chime]);
	}

	/// Le rang mesuré passe avant le coût. La fenêtre réelle tenant quatre ordres
	/// sur huit, `motion` et `osd` doivent être déjà lues quand la caméra rend la
	/// session, et `image` — qui répond un bloc inconnu et efface ses valeurs —
	/// doit être la dernière à partir, ou la première à rester sur le quai.
	#[test]
	fn motion_and_osd_pass_before_the_stays_and_image_after() {
		let entries = [
			Family::Audio,
			Family::Image,
			Family::Ntp,
			Family::Osd,
			Family::Motion,
			Family::Chime,
		];
		let plan = build_plan(&entries, COMMAND_BUDGET);
		let head: Vec<&str> = plan.families.iter().take(2).map(|f| f.slug()).collect();
		assert_eq!(head, vec!["osd", "motion"], "les deux families qui servent entrent les premières");
		assert_eq!(
			*plan.families.last().expect("le plan n'est pas vide"),
			Family::Image,
			"`image` ferme la marche"
		);
		assert!(plan.commands <= COMMAND_BUDGET, "{} commands", plan.commands);

		// Fenêtre plus étroite : c'est encore `image` qui reste sur le quai, et
		// non l'une des deux premières.
		let plan = build_plan(&entries, 4);
		assert!(
			!plan.families.contains(&Family::Image),
			"{:?} — la famille inutile est écartée avant les autres",
			plan.families
		);
		assert_eq!(plan.families.len(), 4, "les quatre qui rapportent tiennent : {:?}", plan.families);
	}

	/// La borne est **haute** : aucun plan ne la dépasse, et une famille chère en
	/// tête n'affame pas les autres.
	#[test]
	fn the_plan_not_exceeds_never_the_bound() {
		let toutes: Vec<Family> = Family::ALL.to_vec();
		let plan = build_plan(&toutes, COMMAND_BUDGET);
		assert!(
			plan.commands <= COMMAND_BUDGET,
			"{} commands pour une borne de {}",
			plan.commands,
			COMMAND_BUDGET
		);
		assert!(plan.skipped > 0, "seize families coûtent plus que la borne");

		// `chime` (5) devant des familles à une commande : le plan les paie de
		// la moins chère à la plus chère, donc les petites passent en premier.
		let plan = build_plan(&[Family::Chime, Family::Audio, Family::Pir], 7);
		assert_eq!(
			plan.families,
			vec![Family::Audio, Family::Pir, Family::Chime],
			"1 + 1 + 5 = 7, la borne est atteinte sans être dépassée"
		);
		let plan = build_plan(&[Family::Chime, Family::Audio, Family::Pir], 6);
		assert_eq!(
			plan.families,
			vec![Family::Audio, Family::Pir],
			"la famille chère est sautée, pas la fenêtre entière"
		);
		assert_eq!(plan.skipped, 1);

		// `ai` ne coûte rien : il reste dans le plan, et son motif avec lui.
		let plan = build_plan(&[Family::Ai], 0);
		assert_eq!(plan.families, vec![Family::Ai], "une famille sans commande ne mange pas la borne");
	}

	/// Une caméra sans MQTT ne lance rien : personne n'attend la lecture.
	#[test]
	fn without_mqtt_the_burst_not_part_not() {
		on_worker(|| Box::pin(without_mqtt()));
	}

	async fn without_mqtt() {
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config("cam-sans-broker"),
			CancellationToken::new(),
			None,
		));
		let t = trigger(Arc::clone(&handle), Wake::Press).expect("la garde s'ouvre quand même");
		t.await.expect("pas de panique");
		assert_eq!(handle.wake_lock().count(), 0);
	}
}
