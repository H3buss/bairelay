//! Salve de lecture déclenchée par le **réveil spontané** de la caméra.
//!
//! # Pourquoi ce module existe
//!
//! La batterie est l'urgence du terrain : plus aucun minuteur ne lit la caméra
//! (`idle_disconnect = true`, `battery_update` réglé sur « jamais »), donc le
//! niveau de batterie ne remontait plus que si une personne demandait
//! `query/settings`. Or la caméra, elle, se réveille toute seule : appui sur le
//! bouton, remontée de mouvement, enregistrement poussé sur le canal du faux
//! cloud. Chaque réveil est une fenêtre où l'on peut lire **gratuitement** — la
//! radio est déjà ouverte.
//!
//! Une salve, c'est cette fenêtre : quelques commandes, payées au réveil, jamais
//! ailleurs.
//!
//! # Les quatre règles, et pourquoi elles sont verrouillées par des tests
//!
//! 1. **Un réveil = une salve.** La garde ([`BurstGuard`]) refuse une seconde
//!    salve tant que la première tourne *et* pendant [`GUARD_WINDOW`] après
//!    elle. Trois sources peuvent annoncer le même réveil (la session, le
//!    webhook, la poussée) : sans cela, on paierait trois salves d'un coup.
//! 2. **Jamais périodique, jamais au démarrage.** Ce module n'a aucune
//!    `interval`, aucune `sleep` de cadencement, et n'est appelé d'aucun
//!    chemin de démarrage : la mémoire de
//!    [`crate::family_state::familles_prouvees`] suffit à publier la découverte
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
//!    et écrite dans le journal avec ce qui a été sauté. Une famille chère
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
//! famille refusée [`FAMILY_MAX_REFUSALS`] réveils de suite sort du plan
//! ([`BurstGuard::famille_saturee`]) : on ne rachète pas cent fois la même
//! porte fermée. Le motif de chaque refus reste publié par
//! [`crate::settings::read_family`] lui-même.
//!
//! # La mémoire des familles
//!
//! La salve ne dévine aucune famille : elle relit celles dont la modifiabilité a
//! **déjà été prouvée** ([`crate::family_state`]), et écrit cette mémoire
//! **une seule fois** par salve — un seul appel à
//! [`CameraHandle::mark_families_read`] en fin de salve, qui est le seul
//! chemin qui touche le disque.

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

/// Fenêtre pendant laquelle un second réveil ne relance **pas** de salve.
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
/// tenir le réveil ouvert pour ça : au-delà, ce qui est lu est publié, le reste
/// attendra le prochain réveil.
const TIME_BUDGET: Duration = Duration::from_secs(25);

/// Combien de temps attendre que la session Baïchuan soit établie.
///
/// Le réveil tient le verrou, la boucle de connexion de la caméra fait le
/// reste : vingt secondes couvrent un établissement lent sur Wi-Fi sans
/// immobiliser la caméra si elle est morte.
const SESSION_WAIT: Duration = Duration::from_secs(20);

/// Période de la sonde d'attente de session.
const SESSION_POLL: Duration = Duration::from_millis(200);

/// Après autant de réveils consécutifs sans réponse à la trame 253, la salve
/// arrête de la payer.
///
/// Le pendant, dans la salve, du verrouillage `battery_unsupported` du poller —
/// que la salve ne peut pas attendre : avec `battery_update` réglé sur
/// « jamais », le poller ne tick plus et ne verrouille donc jamais rien. Une
/// caméra secteur ne mérite pas une commande perdue à chaque appui, indéfiniment.
const BATTERY_MAX_REFUSALS: usize = 3;

/// Après autant de réveils consécutifs à refuser une famille, la salve cesse de
/// la payer — jusqu'au prochain redémarrage du pont.
///
/// Trois, et non un : le terrain montre des `400` qui s'effacent d'un réveil à
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
/// peu pour ne pas brûler un réveil contre un firmware muet.
const MAX_CONSECUTIVE_REFUSALS: usize = 2;

/// Ce qui a réveillé la caméra. Publié avec l'état de la salve : « est-ce que
/// l'appui du bouton me coûte une salve ? » est la première question de
/// l'opératrice qui soupçonne la batterie de partir vite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
	/// Appui sur le bouton de la sonnette.
	Appui,
	/// Remontée de mouvement (alarme de session, alarme relayée, PIR webhook).
	Mouvement,
	/// Trame entrante sur le canal du faux cloud (poussée TCP).
	Poussee,
}

impl Wake {
	/// Le mot court publié dans l'état de salve.
	pub(crate) fn slug(self) -> &'static str {
		match self {
			Wake::Appui => "appui",
			Wake::Mouvement => "mouvement",
			Wake::Poussee => "poussee",
		}
	}
}

/// Pourquoi la salve s'est arrêtée — et donc ce que l'opératrice lira sur
/// `status/<cam>/status/burst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
	/// Pas de MQTT : rien à publier, donc rien à lire.
	SansMqtt,
	/// Un flux est en cours : la règle 3.
	Flux,
	/// Aucune session Baïchuan n'a été établie pendant [`SESSION_WAIT`].
	SansSession,
	/// [`MAX_CONSECUTIVE_REFUSALS`] refus d'affilée, ou une tentative impossible
	/// ([`Verdict::Unknown`]) : la salve s'arrête là, motifs publiés.
	Refus,
	/// La borne [`COMMAND_BUDGET`] a fait sauter au moins une famille.
	Budget,
	/// [`TIME_BUDGET`] dépassée.
	Duree,
	/// Le pont s'arrête (jeton de la caméra annulé).
	Annulee,
	/// Tout le plan est passé.
	Terminee,
}

impl Stop {
	/// Le mot court publié dans l'état de salve.
	pub(crate) fn slug(self) -> &'static str {
		match self {
			Stop::SansMqtt => "sans-mqtt",
			Stop::Flux => "flux",
			Stop::SansSession => "sans-session",
			Stop::Refus => "refus",
			Stop::Budget => "budget",
			Stop::Duree => "duree",
			Stop::Annulee => "annulee",
			Stop::Terminee => "terminee",
		}
	}
}

/// Ce qu'une caméra sait déjà faire, entre deux salves.
///
/// Un seul exemplaire par caméra, posée sur le [`CameraHandle`] : c'est l'objet
/// qui répond à « ai-je déjà payé une salve pour ce réveil ? », question qui se
/// pose simultanément depuis la session, le webhook et la poussée.
#[derive(Debug)]
pub(crate) struct BurstGuard {
	/// Une salve tourne-t-elle en ce moment ?
	en_cours: AtomicBool,
	/// Instant de départ de la dernière salve acceptée.
	derniere: Mutex<Option<Instant>>,
	/// Fenêtre de refus après une salve ([`GUARD_WINDOW`] en production).
	fenetre: Duration,
	/// Refus consécutifs de la trame 253, vus par les salves.
	batterie_refus: AtomicUsize,
	/// Refus consécutifs par famille, vus par les salves de **cette** caméra.
	///
	/// En mémoire seulement, et c'est voulu : la mémoire du lot
	/// ([`crate::family_state`]) dit si une famille est *écrivable*, question
	/// durable ; ici la question est « est-ce que je continue à payer une
	/// lecture que cette caméra refuse depuis trois réveils ? », question de
	/// séance. Un redémarrage rouvre donc les trois tentatives — c'est exactement
	/// ce qu'un redémarrage veut dire pour l'opératrice.
	refus_familles: Mutex<HashMap<Family, usize>>,
}

impl BurstGuard {
	/// Garde neuve, avec la fenêtre de refus à appliquer.
	pub(crate) fn nouvelle(fenetre: Duration) -> Self {
		BurstGuard {
			en_cours: AtomicBool::new(false),
			derniere: Mutex::new(None),
			fenetre,
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
	pub(crate) fn prendre(&self, maintenant: Instant) -> bool {
		if self.en_cours.swap(true, Ordering::SeqCst) {
			return false;
		}
		let mut derniere = self.derniere.lock().unwrap_or_else(|p| p.into_inner());
		if let Some(depuis) = *derniere {
			if maintenant.duration_since(depuis) < self.fenetre {
				drop(derniere);
				self.en_cours.store(false, Ordering::SeqCst);
				return false;
			}
		}
		*derniere = Some(maintenant);
		drop(derniere);
		true
	}

	/// Une salve vient de se terminer : la garde redevient disponible, mais la
	/// fenêtre, elle, court toujours.
	pub(crate) fn liberer(&self) {
		self.en_cours.store(false, Ordering::SeqCst);
	}

	/// Une salve tourne-t-elle ? Observé par les tests : la production décide
	/// avec [`BurstGuard::prendre`], qui réserve et répond dans le même élan.
	#[cfg(test)]
	pub(crate) fn en_cours(&self) -> bool {
		self.en_cours.load(Ordering::SeqCst)
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
	pub(crate) fn note_family_refused(&self, famille: Family) -> usize {
		let mut refus = self.refus_familles.lock().unwrap_or_else(|p| p.into_inner());
		let compteur = refus.entry(famille).or_insert(0);
		*compteur += 1;
		*compteur
	}

	/// Une famille vient de répondre : elle n'est plus en dette. La clé reste
	/// en place à `0` (une clé par famille, quinze au plus) plutôt que de
	/// déséquilibrer la table à chaque salve.
	pub(crate) fn note_family_read(&self, famille: Family) {
		let mut refus = self.refus_familles.lock().unwrap_or_else(|p| p.into_inner());
		refus.insert(famille, 0);
	}

	/// Faut-il cesser de payer cette famille ? [`FAMILY_MAX_REFUSALS`] refus de
	/// suite, et plus rien jusqu'au prochain succès — qui, par définition, ne
	/// peut plus venir tant que la famille est écartée : c'est le redémarrage du
	/// pont qui rejoue la chance, et c'est assumé.
	pub(crate) fn famille_saturee(&self, famille: Family) -> bool {
		self.refus_familles
			.lock()
			.unwrap_or_else(|p| p.into_inner())
			.get(&famille)
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

/// Le plan d'une salve : ce qui sera lu, ce que ça coûte, ce qui a été sauté.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BurstPlan {
	/// Les familles qui seront relues, dans l'ordre de lecture.
	pub(crate) familles: Vec<Family>,
	/// Total des commandes évalué ([`cout`]).
	pub(crate) commandes: usize,
	/// Familles écartées parce que la borne ne les payait pas.
	pub(crate) sautees: usize,
}

/// Combien de commandes radio une famille coûte à relire.
///
/// Les nombres viennent de [`crate::settings::sync_family`], branche par
/// branche — pas d'une estimation : `chime` est la seule famille à cinq commandes
/// (483, 484, 485, 486, 609), et `ai` n'en coûte aucune parce qu'aucune balise
/// d'activation n'a jamais été documentée.
pub(crate) fn cout(famille: Family) -> usize {
	match famille {
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
fn rang(famille: Family) -> u8 {
	match famille {
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
///   compte est le nombre de familles revenues vérifiées pour un réveil de la
///   caméra. Le tri est **stable** : à rang et coût égaux l'ordre reçu est
///   conservé, et deux salves sur le même ensemble de familles se ressemblent.
///
/// C'est aussi ce qui rend la borne **haute** au sens strict :
/// [`BurstPlan::commandes`] ne dépasse jamais `budget`.
pub(crate) fn build_plan(familles: &[Family], budget: usize) -> BurstPlan {
	let mut payees: Vec<(u8, usize, Family)> =
		familles.iter().copied().map(|f| (rang(f), cout(f), f)).collect();
	payees.sort_by_key(|(rangement, prix, _)| (*rangement, *prix));
	let mut plan = BurstPlan { familles: Vec::new(), commandes: 0, sautees: 0 };
	for (_, prix, famille) in payees {
		if plan.commandes + prix > budget {
			plan.sautees += 1;
			continue;
		}
		plan.commandes += prix;
		plan.familles.push(famille);
	}
	plan
}

/// Les familles que la mémoire dit prouvées, moins la batterie.
///
/// La batterie est lue à part, en premier : c'est pour elle que la salve existe,
/// et son niveau se publie sur un topic que `sync_family` ne remplit pas.
pub(crate) fn familles_a_lire(camera: &str) -> Vec<Family> {
	crate::family_state::familles_prouvees(camera)
		.into_iter()
		.filter(|f| *f != Family::Battery)
		.collect()
}

/// Déclencher une salve si le réveil a droit à la sienne.
///
/// Appelé **après** la publication de l'événement qui l'a déclenché — jamais
/// avant : un appui doit être publié avant la première commande de lecture, sinon
/// l'automatisation de l'opératrice découvre l'appui avec le retard de toute la
/// salve. La décision de partir est **ici**, synchrone, parce que deux réveils à
/// la milliseconde près doivent donner une seule salve.
///
/// Rend la poignée de la tâche pour les tests ; les appelants de production
/// l'ignorent : la salve est détachée par construction.
pub(crate) fn trigger(handle: Arc<CameraHandle>, raison: Wake) -> Option<JoinHandle<()>> {
	if !handle.salve().prendre(Instant::now()) {
		tracing::debug!(
			camera = %handle.name(),
			raison = raison.slug(),
			"réveil sans salve : une salve tourne déjà, ou ce réveil tombe dans la fenêtre du précédent"
		);
		return None;
	}
	tracing::info!(
		camera = %handle.name(),
		raison = raison.slug(),
		budget = COMMAND_BUDGET,
		"salve de réveil engagée"
	);
	Some(tokio::spawn(tache(handle, raison)))
}

/// Le corps de la salve, une fois la garde réservée.
async fn tache(handle: Arc<CameraHandle>, raison: Wake) {
	let _fin = BurstRelease(Arc::clone(handle.salve()));

	let Some(mqtt) = handle.mqtt_client() else {
		let arret = Stop::SansMqtt;
		tracing::debug!(
			camera = %handle.name(),
			raison = raison.slug(),
			etat = arret.slug(),
			"salve sans MQTT : rien à publier, donc rien à lire"
		);
		return;
	};
	let mqtt = SharedMqttClient::clone(mqtt);
	let prefix = handle.topic_prefix().to_string();
	let publisher = StatusPublisher::new(&mqtt, &prefix, handle.name());

	// Règle 3 : jamais pendant un flux.
	if handle.flux_en_cours() {
		tracing::info!(
			camera = %handle.name(),
			raison = raison.slug(),
			"salve refusée : un flux est en cours"
		);
		publier(&mqtt, &prefix, handle.name(), raison, Stop::Flux, 0, 0).await;
		return;
	}

	// Tenir le réveil : sans ce verrou, `idle_disconnect` rendrait la caméra
	// pendant que nous lisons, et la salve perdrait sa propre session.
	let _verrou = handle.wake_lock().acquire();

	let Some(bc) = await_session(&handle).await else {
		tracing::info!(
			camera = %handle.name(),
			raison = raison.slug(),
			salle = ?SESSION_WAIT,
			"salve sans session Baïchuan : la caméra ne s'est pas laissée joindre"
		);
		publier(&mqtt, &prefix, handle.name(), raison, Stop::SansSession, 0, 0).await;
		return;
	};

	// Les familles que cette caméra refuse depuis [`FAMILY_MAX_REFUSALS`] réveils
	// ne repartent pas au budget : la borne de [`COMMAND_BUDGET`] est trop
	// précieuse pour une porte déjà fermée trois fois.
	let garde = handle.salve();
	let possible = familles_a_lire(handle.name());
	let a_lire: Vec<Family> = possible
		.iter()
		.copied()
		.filter(|f| !garde.famille_saturee(*f))
		.collect();
	let ecartees = possible.len() - a_lire.len();
	let plan = build_plan(&a_lire, COMMAND_BUDGET);
	tracing::info!(
		camera = %handle.name(),
		raison = raison.slug(),
		familles = plan.familles.len(),
		sautees = plan.sautees,
		ecartees,
		commandes = plan.commandes,
		budget = COMMAND_BUDGET,
		"plan de salve"
	);

	let mut lues: Vec<(Family, Verdict)> = Vec::new();
	let mut commandes = 0usize;
	let tournee = tokio::time::timeout(
		TIME_BUDGET,
		salve(&handle, &bc, &publisher, &plan, &mut lues, &mut commandes),
	)
	.await;
	let mut arret = match tournee {
		Ok(arret) => arret,
		Err(_) => Stop::Duree,
	};
	if arret == Stop::Terminee && plan.sautees > 0 {
		arret = Stop::Budget;
	}

	// La seule écriture de la mémoire du lot — un appel, une écriture.
	handle.mark_families_read(&lues).await;

	tracing::info!(
		camera = %handle.name(),
		raison = raison.slug(),
		etat = arret.slug(),
		commandes,
		budget = COMMAND_BUDGET,
		familles = lues.len(),
		modifiables = lues.iter().filter(|(_, v)| *v == Verdict::Writable).count(),
		passageres = lues.iter().filter(|(_, v)| *v == Verdict::Transient).count(),
		"salve de réveil terminée : session rendue"
	);
	publier(&mqtt, &prefix, handle.name(), raison, arret, commandes, lues.len()).await;
	// `_verrou` et `_fin` se libèrent ici : la caméra peut se rendormir.
}

/// La tournée de lecture elle-même : la batterie, puis les familles du plan.
///
/// S'arrête au premier refus, borne atteinte, ou annulation. Le compteur de
/// commandes est un paramètre et non une valeur de retour, comme pour
/// [`crate::settings::sync_all`] : quand la tournée est emportée par
/// [`TIME_BUDGET`], ce qui a déjà été payé doit rester visible dans le journal.
async fn salve(
	handle: &CameraHandle,
	bc: &BcCamera,
	publ: &StatusPublisher<'_>,
	plan: &BurstPlan,
	lues: &mut Vec<(Family, Verdict)>,
	commandes: &mut usize,
) -> Stop {
	let garde = handle.salve();

	// La batterie d'abord : c'est la trame 253, celle qui ne réveille pas la
	// caméra, et la raison pour laquelle la salve existe.
	if garde.batterie_epuisee() {
		tracing::debug!(
			camera = %handle.name(),
			refus = BATTERY_MAX_REFUSALS,
			"salve sans trame 253 : la batterie n'a pas répondu depuis assez de réveils pour arrêter de la payer"
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
		garde.note_battery(issu.is_ok());
		let (famille, verdict) = settings::battery_read(publ, &issu).await;
		lues.push((famille, verdict));
		*commandes += 1;
		// Un `400` sur la trame 253 ne doit pas éteindre la tournée non plus :
		// c'est exactement le cas mesuré où la batterie refuse et où le carillon,
		// juste derrière, aurait répondu. Le « stopper » de la trame 253 a déjà
		// son propre garde-fou ([`BATTERY_MAX_REFUSALS`], compté par
		// [`BurstGuard::note_battery`]).
		if verdict == Verdict::Unknown {
			tracing::info!(
				camera = %handle.name(),
				famille = famille.slug(),
				"salve arrêtée sur la batterie : rien n'a répondu, la session est rendue"
			);
			return Stop::Refus;
		}
	}

	// Refus **d'affilée dans cette tournée** : remis à zéro dès qu'une famille
	// répond. C'est ce compteur qui rend la session, pas celui par famille.
	let mut refus_tournee = 0usize;
	for &famille in &plan.familles {
		let prix = cout(famille);
		if *commandes + prix > COMMAND_BUDGET {
			tracing::info!(
				camera = %handle.name(),
				famille = famille.slug(),
				commandes = *commandes,
				budget = COMMAND_BUDGET,
				"salve arrêtée par la borne de commandes"
			);
			return Stop::Budget;
		}
		*commandes += prix;
		if handle.cancel_token().is_cancelled() {
			return Stop::Annulee;
		}
		let (famille, verdict) = read_family(bc, publ, famille).await;
		lues.push((famille, verdict));
		match verdict {
			// La caméra a dit non : elle est donc vivante et elle répond. Ce
			// refus ne dit **rien** de la famille suivante, et l'historique de
			// cette sonnette est là pour le prouver — la tournée s'arrêtait à
			// quatre commandes et le carillon n'était plus jamais lu. On note,
			// on continue, et on ne dépasse pas [`MAX_CONSECUTIVE_REFUSALS`].
			Verdict::Transient => {
				// Deux compteurs, et pas un : `refus` est celui de **cette
				// famille** à travers les réveils (il décide si on continue de la
				// payer), `tournee` est celui de la tournée en cours (il décide si
				// on rend la session). Les confondre éteindrait la tournée dès la
				// première famille, ou la laisserait courir quinze portes fermées.
				let refus = garde.note_family_refused(famille);
				refus_tournee += 1;
				tracing::info!(
					camera = %handle.name(),
					famille = famille.slug(),
					refus,
					plafond = FAMILY_MAX_REFUSALS,
					tournee = refus_tournee,
					"famille refusée : la salve continue, la suivante n'est pas condamnée"
				);
				if refus_tournee >= MAX_CONSECUTIVE_REFUSALS {
					tracing::info!(
						camera = %handle.name(),
						famille = famille.slug(),
						plafond = MAX_CONSECUTIVE_REFUSALS,
						"salve rendue après deux refus d'affilée : les familles lues restent publiées"
					);
					return Stop::Refus;
				}
			}
			// Rien n'a répondu : ce n'est pas un refus, c'est l'absence. Tenir
			// la session ne peut plus rien rapporter.
			Verdict::Unknown => {
				tracing::info!(
					camera = %handle.name(),
					famille = famille.slug(),
					"salve arrêtée sur une tentative impossible : les familles déjà lues restent publiées, la session est rendue"
				);
				return Stop::Refus;
			}
			// La famille a répondu : elle n'est plus en dette de refus, et la
			// série de refus de la tournée est rompue.
			autre => {
				garde.note_family_read(famille);
				refus_tournee = 0;
				let _ = autre;
			}
		}
	}

	Stop::Terminee
}

/// Attendre que la session Baïchuan de cette caméra soit là.
///
/// Aucune commande n'est émise ici : c'est la boucle de connexion de la caméra
/// qui établit la session, tenue par le verrou que la salve (et le hold du
/// réveil) détient. La sonde est bornée par [`SESSION_WAIT`] et coupée par
/// l'arrêt du pont.
///
/// `pub(crate)` parce que [`crate::polling`] attend la même session pour
/// la même raison : un verrou pris sans session obtenue ne sert à rien.
pub(crate) async fn await_session(handle: &CameraHandle) -> Option<Arc<BcCamera>> {
	let echeance = tokio::time::Instant::now() + SESSION_WAIT;
	loop {
		if let Some(bc) = handle.bc_camera_concrete() {
			return Some(bc);
		}
		if handle.cancel_token().is_cancelled() {
			return None;
		}
		let maintenant = tokio::time::Instant::now();
		if maintenant >= echeance {
			return None;
		}
		let reste = echeance - maintenant;
		tokio::select! {
			_ = handle.cancel_token().cancelled() => return None,
			_ = tokio::time::sleep(reste.min(SESSION_POLL)) => {}
		}
	}
}

/// Publier l'état de la salve : `status/<cam>/status/burst`, conservé.
///
/// Le sujet n'a pas d'entité Home Assistant découverte — volontairement : c'est
/// un topic de diagnostic, et la découverte publierait une entité que
/// l'opératrice n'a pas demandée. Le motif détaillé, famille par famille, est
/// déjà publié sur `status/<famille>/reason` par `reglages`.
async fn publier(
	mqtt: &SharedMqttClient,
	prefix: &str,
	camera: &str,
	raison: Wake,
	etat: Stop,
	commandes: usize,
	familles: usize,
) {
	let topic = format!("{prefix}/{camera}/status/burst");
	let payload = format!(
		r#"{{"raison":"{raison}","etat":"{etat}","commandes":{commandes},"budget":{budget},"familles":{familles}}}"#,
		raison = raison.slug(),
		etat = etat.slug(),
		commandes = commandes,
		budget = COMMAND_BUDGET,
		familles = familles,
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
	fn batterie(reponse: u32) -> BcXml {
		BcXml {
			battery_info: Some(bairelay_neolink_core::bc::xml::BatteryInfo {
				battery_percent: reponse,
				charge_status: "none".to_string(),
				..Default::default()
			}),
			..Default::default()
		}
	}

	/// Le même harnais que `mqtt_dispatch::tests::sur_worker` : une `BcXml`
	/// complète fait plusieurs kilo-octets et chaque commande caméra en brasse au
	/// moins deux copies, donc le corps tourne sur un fil à pile explicite.
	type Corps = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

	fn sur_worker(corps: fn() -> Corps) {
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
				rt.block_on(corps());
			})
			.expect("fil de test à pile explicite")
			.join()
			.expect("la salve ne doit pas paniquer");
	}

	/// Règle 1, et le cœur du lot : deux réveils, une seule salve.
	#[test]
	fn un_reveil_declenche_une_seule_salve() {
		sur_worker(|| Box::pin(une_seule_salve()));
	}

	async fn une_seule_salve() {
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

		let premiere = trigger(Arc::clone(&handle), Wake::Appui).expect("la première salve part");
		// Le second réveil du même réveil : l'alarme webhook qui suit l'appui.
		let seconde = trigger(Arc::clone(&handle), Wake::Mouvement);
		assert!(seconde.is_none(), "un second réveil dans la fenêtre ne paie pas de salve");
		premiere.await.expect("la salve n'a pas paniqué");

		assert_eq!(
			*trace.lock().unwrap(),
			vec![MSG_ID_BATTERY_INFO],
			"une seule salve = une seule trame 253, et rien d'autre"
		);
		let etat = capture
			.published_payloads()
			.into_iter()
			.find(|(t, _)| t == "bairelay/cam-salve/status/burst")
			.expect("l'état de salve est publié");
		let etat = String::from_utf8_lossy(&etat.1).into_owned();
		assert!(etat.starts_with("{\"raison\":\"appui\""), "{etat}");
		assert!(etat.contains("\"commandes\":1"), "{etat}");
		assert!(!etat.contains("\"etat\":\"refus\""), "{etat}");
	}

	/// Pas de réveil = pas une commande. C'est la propriété que la batterie
	/// réclame : sans elle, le pont est juste un sondeur de plus.
	#[test]
	fn aucun_reveil_ne_prie_aucune_commande() {
		sur_worker(|| Box::pin(aucun_reveil()));
	}

	async fn aucun_reveil() {
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
		assert!(trace.lock().unwrap().is_empty(), "aucun réveil, aucune commande caméra");
		assert_eq!(handle.wake_lock().count(), 0, "sans salve, rien ne tient le réveil");
	}

	/// Le `press` doit être **publié** avant la première commande de la salve,
	/// sinon l'automatisation de l'opératrice découvre l'appui avec le retard de
	/// toute la lecture.
	#[test]
	fn le_press_part_avant_la_premiere_commande() {
		sur_worker(|| Box::pin(press_d_abord()));
	}

	async fn press_d_abord() {
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

		// Le chemin de production : le rapport d'appui publie le press, **puis**
		// déclare le réveil.
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
		let position_etat = topics
			.iter()
			.position(|t| t == "bairelay/cam-appui/status/burst")
			.expect("l'état de salve est publié en fin de salve");
		assert!(
			position_press < position_etat,
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
	fn salve_interrompue_publie_et_rend_la_session() {
		sur_worker(|| Box::pin(salve_interrompue()));
	}

	async fn salve_interrompue() {
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
			familles: vec![Family::Motion, Family::Mask],
			commandes: 2,
			sautees: 0,
		};
		let (arret, commandes) = {
			let publisher = StatusPublisher::new(
				handle.mqtt_client().expect("harnais avec MQTT"),
				handle.topic_prefix(),
				handle.name(),
			);
			let bc = handle.bc_camera_concrete().expect("session scriptée");
			let mut lues = Vec::new();
			let mut payees = 0usize;
			let fin = salve(&handle, &bc, &publisher, &plan, &mut lues, &mut payees).await;
			// Les deux familles doivent être passées : la première refusée
			// n'a pas éteint la seconde. C'est la correction du 13/11, mesurée
			// sur la vraie sonnette — `image` en `400` y tue le carillon.
			assert!(lues
				.iter()
				.any(|(f, v)| *f == Family::Motion && *v == Verdict::Transient));
			assert!(
				lues.iter()
					.any(|(f, v)| *f == Family::Mask && *v == Verdict::Transient),
				"la famille qui suit un `400` est tentée quand même ; observé : {lues:?}"
			);
			(fin, payees)
		};
		assert_eq!(
			arret,
			Stop::Refus,
			"deux refus d'affilée rendent la session ([MAX_CONSECUTIVE_REFUSALS])"
		);
		assert_eq!(
			commandes, 3,
			"trame 253 (1) + les deux familles refusées (1 + 1) : les deux lectures refusées ont bien été payées, donc tentées"
		);
		let topics = capture.published_topics();
		assert!(
			topics.iter().any(|t| t.ends_with("status/motion/reason")),
			"le motif de la première famille refusée est publié ; observé : {topics:?}"
		);
		assert!(
			topics.iter().any(|t| t.ends_with("status/mask/reason")),
			"le motif de la seconde aussi, puisque sa lecture a été tentée ; observé : {topics:?}"
		);

		// Et la session rendue. `trigger` est le seul chemin qui prend le
		// verrou de réveil — c'est donc lui qu'on teste pour la restitution,
		// avec une session neuve : le script ci-dessus est épuisé, et une
		// mock épuisée ne répond plus, ce qui n'apprendrait rien.
		let mock = MockConnection::new()
			.expect_msg(MSG_ID_BATTERY_INFO)
			.reply_with(|req| reply_200_xml(req, batterie(35)))
			.build()
			.await;
		handle.set_bc_camera_for_test(Arc::new(BcCamera::from_mock_connection(mock).await));
		let tache = trigger(Arc::clone(&handle), Wake::Poussee).expect("salve autorisée");
		tache.await.expect("pas de panique");
		assert_eq!(
			handle.wake_lock().count(),
			0,
			"une salve terminée, y compris interrompue, a rendu le verrou de réveil"
		);
		assert!(!handle.salve().en_cours(), "la garde est libérée");
		assert!(
			capture.published_topics().iter().any(|t| t.ends_with("status/burst")),
			"la salve détachée a publié son état : {:?}",
			capture.published_topics()
		);
	}

	/// La garde refuse la seconde salve dans la fenêtre et l'autorise après.
	#[test]
	fn la_garde_tient_la_fenetre() {
		let garde = BurstGuard::nouvelle(Duration::from_millis(500));
		let debut = Instant::now();
		assert!(garde.prendre(debut));
		assert!(!garde.prendre(debut + Duration::from_millis(10)), "la fenêtre court");
		garde.liberer();
		assert!(
			!garde.prendre(debut + Duration::from_millis(60)),
			"une salve terminée dans la fenêtre ne relance rien"
		);
		assert!(garde.prendre(debut + Duration::from_millis(600)), "fenêtre passée");
	}

	/// Une famille que la caméra refuse depuis [`FAMILY_MAX_REFUSALS`] réveils ne
	/// repart plus au budget ; une famille qui répond efface sa dette.
	///
	/// C'est la moitié qui protège la batterie : sans elle, « continuer après un
	/// `400` » signifierait racheter quatre portes fermées à chaque appui, tous
	/// les jours, sur une sonnette sur pile.
	#[test]
	fn une_famille_refusee_trois_fois_sort_du_plan() {
		let garde = BurstGuard::nouvelle(Duration::from_millis(500));
		assert!(!garde.famille_saturee(Family::Image), "neuve, elle est dans le plan");
		assert_eq!(garde.note_family_refused(Family::Image), 1);
		assert_eq!(garde.note_family_refused(Family::Image), 2);
		assert!(!garde.famille_saturee(Family::Image), "deux refus, elle y est encore");
		assert_eq!(garde.note_family_refused(Family::Image), FAMILY_MAX_REFUSALS);
		assert!(garde.famille_saturee(Family::Image), "trois refus d'affilée, stop");

		// Les autres familles n'y sont pour rien : `image` refuse et `chime`
		// se présente quand même au réveil suivant.
		assert!(!garde.famille_saturee(Family::Chime), "une famille n'entraîne pas les autres");

		// Et un succès rouvre la porte : la dette est effacée.
		garde.note_family_read(Family::Image);
		assert!(!garde.famille_saturee(Family::Image), "une famille qui a répondu repart à zéro");

		// Le plan ne la paie plus : à budget compté, `image` (1) et `chime` (5)
		// sur une borne de 6, moins `image` écartée, laisse les 5 de `chime`.
		let garde = BurstGuard::nouvelle(Duration::from_millis(500));
		for _ in 0..FAMILY_MAX_REFUSALS {
			garde.note_family_refused(Family::Image);
		}
		let possibles = [Family::Image, Family::Chime];
		let a_lire: Vec<Family> = possibles
			.iter()
			.copied()
			.filter(|f| !garde.famille_saturee(*f))
			.collect();
		assert_eq!(a_lire, vec![Family::Chime], "la famille saturée ne repart pas au budget");
		assert_eq!(build_plan(&a_lire, 6).familles, vec![Family::Chime]);
	}

	/// Le rang mesuré passe avant le coût. La fenêtre réelle tenant quatre ordres
	/// sur huit, `motion` et `osd` doivent être déjà lues quand la caméra rend la
	/// session, et `image` — qui répond un bloc inconnu et efface ses valeurs —
	/// doit être la dernière à partir, ou la première à rester sur le quai.
	#[test]
	fn motion_et_osd_passent_avant_le_reste_et_image_apres() {
		let entrees = [
			Family::Audio,
			Family::Image,
			Family::Ntp,
			Family::Osd,
			Family::Motion,
			Family::Chime,
		];
		let plan = build_plan(&entrees, COMMAND_BUDGET);
		let tete: Vec<&str> = plan.familles.iter().take(2).map(|f| f.slug()).collect();
		assert_eq!(tete, vec!["osd", "motion"], "les deux familles qui servent entrent les premières");
		assert_eq!(
			*plan.familles.last().expect("le plan n'est pas vide"),
			Family::Image,
			"`image` ferme la marche"
		);
		assert!(plan.commandes <= COMMAND_BUDGET, "{} commandes", plan.commandes);

		// Fenêtre plus étroite : c'est encore `image` qui reste sur le quai, et
		// non l'une des deux premières.
		let plan = build_plan(&entrees, 4);
		assert!(
			!plan.familles.contains(&Family::Image),
			"{:?} — la famille inutile est écartée avant les autres",
			plan.familles
		);
		assert_eq!(plan.familles.len(), 4, "les quatre qui rapportent tiennent : {:?}", plan.familles);
	}

	/// La borne est **haute** : aucun plan ne la dépasse, et une famille chère en
	/// tête n'affame pas les autres.
	#[test]
	fn le_plan_ne_depasse_jamais_la_borne() {
		let toutes: Vec<Family> = Family::ALL.to_vec();
		let plan = build_plan(&toutes, COMMAND_BUDGET);
		assert!(
			plan.commandes <= COMMAND_BUDGET,
			"{} commandes pour une borne de {}",
			plan.commandes,
			COMMAND_BUDGET
		);
		assert!(plan.sautees > 0, "seize familles coûtent plus que la borne");

		// `chime` (5) devant des familles à une commande : le plan les paie de
		// la moins chère à la plus chère, donc les petites passent en premier.
		let plan = build_plan(&[Family::Chime, Family::Audio, Family::Pir], 7);
		assert_eq!(
			plan.familles,
			vec![Family::Audio, Family::Pir, Family::Chime],
			"1 + 1 + 5 = 7, la borne est atteinte sans être dépassée"
		);
		let plan = build_plan(&[Family::Chime, Family::Audio, Family::Pir], 6);
		assert_eq!(
			plan.familles,
			vec![Family::Audio, Family::Pir],
			"la famille chère est sautée, pas la fenêtre entière"
		);
		assert_eq!(plan.sautees, 1);

		// `ai` ne coûte rien : il reste dans le plan, et son motif avec lui.
		let plan = build_plan(&[Family::Ai], 0);
		assert_eq!(plan.familles, vec![Family::Ai], "une famille sans commande ne mange pas la borne");
	}

	/// Une caméra sans MQTT ne lance rien : personne n'attend la lecture.
	#[test]
	fn sans_mqtt_la_salve_ne_part_pas() {
		sur_worker(|| Box::pin(sans_mqtt()));
	}

	async fn sans_mqtt() {
		let handle = Arc::new(CameraHandle::new(
			minimal_camera_config("cam-sans-broker"),
			CancellationToken::new(),
			None,
		));
		let t = trigger(Arc::clone(&handle), Wake::Appui).expect("la garde s'ouvre quand même");
		t.await.expect("pas de panique");
		assert_eq!(handle.wake_lock().count(), 0);
	}
}
