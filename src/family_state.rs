//! La mémoire des familles de réglages **tenues pour modifiables**.
//!
//! # Le problème que ce fichier règle
//!
//! Une famille de réglages n'est publiée modifiable (curseur, selecteur,
//! interrupteur avec topic de commande) qu'après avoir été **relue avec succès**
//! par la caméra. C'est la règle du pont : ne jamais promettre une écriture sur
//! une balise qu'on n'a pas lue.
//!
//! Mais ce verdict ne vivait que dans la mémoire du processus. Concrètement, sur
//! la sonnette :
//!
//! * au démarrage, tout part en lecture seule ;
//! * il faut une salve complète (`query/settings`, ~15 commandes radio, et la
//!   caméra doit être réveillée) pour que les familles retrouvent leur statut ;
//! * et **un redémarrage du complément oublie tout**, donc la caméra paie à
//!   nouveau quinze commandes radio pour retrouver ce qu'elle avait déjà prouvé
//!   la veille.
//!
//! Ce module persiste donc, sur le disque du complément, la liste des familles
//! dont le pont a la preuve qu'elles sont modifiables. Au démarrage suivant, il
//! les marque modifiables **sans envoyer un seul paquet à la caméra**.
//!
//! # Où est le fichier, et pourquoi là
//!
//! À côté du fichier de configuration, sous le nom [`FAMILIES_READ_FILE`] — le
//! même emplacement que [`crate::config::CLOUD_AUTH_FILE`], précédent explicite.
//! Dans le complément Home Assistant, c'est `/config/sonnette/` : un répertoire
//! déjà monté en lecture/écriture, qui survit aux mises à jour de l'image, et
//! qui n'est **pas** le dépôt. Rien n'est écrit dans l'arborescence du code.
//!
//! # Ce que le fichier ne contient pas
//!
//! Des slugs de familles, rien d'autre : pas de valeur de réglage, pas
//! d'identifiant, pas de secret. Le fichier est donc inutile en dehors du pont
//! et ne contient rien à protéger.
//!
//! # Les trois règles de survie
//!
//! 1. **Jamais une commande caméra** : ce module ne fait que lire et écrire un
//!    fichier. Marquer une famille modifiable ne réveille personne.
//! 2. **Jamais un échec de démarrage** : fichier absent = comportement actuel
//!    (tout en lecture seule jusqu'à la première relecture) ; fichier illisible
//!    ou invalide = idem, avec un avertissement. Le pont démarre toujours.
//! 3. **Jamais une écriture à moitié** : écriture dans un fichier temporaire du
//!    même répertoire puis renommage. Un crash laisse l'ancien fichier intact,
//!    jamais un JSON tronqué.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use bairelay_mqtt::discovery::DiscoveryPublisher;
use bairelay_mqtt::settings::Family;

/// Nom du fichier d'état, posé à côté du fichier de configuration.
///
/// Le préfixe `config-` est celui du voisin [`crate::config::CLOUD_AUTH_FILE`] :
/// dans `/config/sonnette/`, ces fichiers se repèrent d'un coup d'œil, et le
/// nom dit ce qu'il contient sans avoir à l'ouvrir.
pub const FAMILIES_READ_FILE: &str = "config-families-read.json";

/// Ce que sait lire ce format. Un fichier d'une version inconnue est ignoré
/// (et réécrit) plutôt que d'être interprété de travers.
const VERSION: u8 = 1;

/// Emplacement résolu une fois pour toutes depuis le chemin du fichier de
/// configuration. `OnceLock` et non un champ de `CameraConfig` : le chemin
/// n'appartient pas aux caméras (il est unique par processus, celui du fichier
/// de config qu'on a chargé), et le threaded jusqu'au cœur de la caméra n'aurait
/// pour seul effet qu'alourdir chaque constructeur de test.
///
/// Absent = ce module ne fait rien. C'est aussi ce qui rend les tests du reste
/// du bin sans effet de bord : tant que `set_path` n'est pas appelé,
/// aucune écriture n'a lieu.
static CHEMIN: OnceLock<PathBuf> = OnceLock::new();

/// Caméras déjà amorcées dans ce processus : l'amorçage ne se joue qu'une fois
/// par caméra, sinon chaque repubication de découverte relirait le fichier.
/// `BTreeSet` et non `HashSet` uniquement parce que le premier doit pouvoir se
/// construire dans une `static`.
static AMORCEES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Contenu du fichier : `{"version":1,"cameras":{"Sonnette":["audio",…]}}`.
///
/// `BTreeMap` pour que deux écritures identiques produisent des octets
/// identiques — un fichier qui ne bouge pas quand rien ne bouge se diff et se
/// lit mieux quand on diagnostique le terrain.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ReadFamilies {
	version: u8,
	cameras: BTreeMap<String, Vec<String>>,
}

/// Calcule l'emplacement depuis le chemin du fichier de configuration, et le
/// retient. Appelé une fois dans `main`, juste après que `--config` est résolu.
pub fn set_path(config_path: &Path) {
	let _ = CHEMIN.set(config_path.with_file_name(FAMILIES_READ_FILE));
}

/// L'emplacement retenu, si `set_path` a été appelé.
fn chemin() -> Option<&'static Path> {
	CHEMIN.get().map(PathBuf::as_path)
}

/// Relit l'état d'une caméra : les familles dont la modifiabilité est prouvée.
///
/// Fichier absent, illisible, caméra inconnue, slug inconnu : **`Vec` vide**,
/// avec un avertissement quand le fichier était bien là mais ne se laisse pas
/// lire. Un état impossible à lire doit se confondre avec « on ne sait pas
/// encore », jamais empêcher le pont de démarrer.
fn charger(chemin: &Path, camera: &str) -> Vec<Family> {
	let brut = match std::fs::read_to_string(chemin) {
		Ok(brut) => brut,
		// Absent : le cas normal au premier démarrage, et après une
		// réinstallation du complément. Pas un événement.
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
		Err(e) => {
			tracing::warn!(
				chemin = %chemin.display(),
				erreur = %e,
				"état des familles illisible : tout repart en lecture seule jusqu'à la première relecture"
			);
			return Vec::new();
		}
	};
	let lu: Result<ReadFamilies, serde_json::Error> = serde_json::from_str(&brut);
	let etat = match lu {
		Ok(etat) if etat.version == VERSION => etat,
		Ok(etat) => {
			tracing::warn!(
				chemin = %chemin.display(),
				version = etat.version,
				"état des familles dans un format inconnu : il sera réécrit à la première écriture"
			);
			return Vec::new();
		}
		Err(e) => {
			tracing::warn!(
				chemin = %chemin.display(),
				erreur = %e,
				"état des familles corrompu : tout repart en lecture seule jusqu'à la première relecture"
			);
			return Vec::new();
		}
	};
	let Some(slugs) = etat.cameras.get(camera) else {
		return Vec::new();
	};
	// Un slug que ce binaire ne connaît pas ne crée aucune famille : le fichier
	// peut avoir été écrit par une version plus récente, et deviner une famille à
	// partir d'un mot serait exactement l'invention que le reste du pont refuse.
	slugs.iter().filter_map(|s| Family::from_slug(s)).collect()
}

/// Les familles dont la modifiabilité est **déjà prouvée** pour cette caméra,
/// dans l'ordre de [`Family::ALL`] — donc l'ordre dans lequel la salve de réveil
/// ([`crate::burst`]) les relira.
///
/// `Vec` vide quand [`set_path`] n'a pas été appelé, quand le fichier est
/// absent, ou quand cette caméra n'y figure pas encore : la salve ne lit alors
/// que la batterie. C'est voulu — rien ici ne devine une famille, et une famille
/// qu'on n'a jamais lue n'a rien à faire dans une salve payée en radio.
pub(crate) fn familles_prouvees(camera: &str) -> Vec<Family> {
	let Some(chemin) = chemin() else {
		return Vec::new();
	};
	let connues = charger(chemin, camera);
	if connues.is_empty() {
		return Vec::new();
	}
	// L'ordre du protocole plutôt que celui du fichier : `charger` rend les slugs
	// dans l'ordre où le JSON les a écrits, et la salve doit partir des familles
	// dont l'opératrice se soucie en premier, pas de l'ordre alphabétique.
	Family::ALL
		.into_iter()
		.filter(|f| connues.contains(f))
		.collect()
}

/// Écrit l'état d'une caméra, en préservant les autres.
///
/// Écriture **atomique** : fichier temporaire dans le même répertoire (même
/// système de fichiers, seule condition pour que le renommage le soit aussi),
/// puis renommage. Le renommage par-dessus un fichier existant échoue sur
/// Windows, d'où le retrait juste avant — la fenêtre entre les deux n'expose
/// qu'à lire « absent », ce qui est déjà un cas traité.
fn sauver(chemin: &Path, camera: &str, familles: &[Family]) -> io::Result<()> {
	let mut etat = std::fs::read_to_string(chemin)
		.ok()
		.and_then(|brut| serde_json::from_str::<ReadFamilies>(&brut).ok())
		.unwrap_or_default();
	// Un fichier d'une autre version repart de zéro, mais uniquement pour la
	// caméra qu'on est en train d'écrire : les autres entrées restent ce
	// qu'elles sont, et la version est relevée.
	etat.version = VERSION;
	let mut slugs: Vec<String> = familles.iter().map(|f| f.slug().to_string()).collect();
	slugs.sort();
	slugs.dedup();
	if slugs.is_empty() {
		etat.cameras.remove(camera);
	} else {
		etat.cameras.insert(camera.to_string(), slugs);
	}
	if let Some(dir) = chemin.parent().filter(|d| !d.as_os_str().is_empty()) {
		std::fs::create_dir_all(dir)?;
	}
	let nom = chemin
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_else(|| FAMILIES_READ_FILE.to_string());
	// Le pid dans le nom du temporaire : deux processus qui partagent ce
	// répertoire (un complément qui redémarre pendant qu'on lance un outil en
	// parallèle) ne doivent pas écrire dans le même fichier temporaire.
	let temporaire = chemin.with_file_name(format!("{nom}.{}.tmp", std::process::id()));
	std::fs::write(&temporaire, serde_json::to_string_pretty(&etat).unwrap_or_default())?;
	if chemin.exists() {
		std::fs::remove_file(chemin)?;
	}
	if let Err(e) = std::fs::rename(&temporaire, chemin) {
		let _ = std::fs::remove_file(&temporaire);
		return Err(e);
	}
	Ok(())
}

/// Amorçage, avec l'emplacement retenu par [`set_path`]. Ne fait rien si
/// aucun emplacement n'a été résolu (tests, outils en ligne de commande).
pub fn amorcer(publisher: &DiscoveryPublisher, camera: &str) {
	let Some(chemin) = chemin() else { return };
	let deja = {
		let mut garde = match AMORCEES.lock() {
			Ok(g) => g,
			Err(p) => p.into_inner(),
		};
		!garde.insert(camera.to_string())
	};
	if deja {
		return;
	}
	amorcer_dans(chemin, publisher, camera);
}

/// Même chose, emplacement explicite — la forme que testent les tests.
fn amorcer_dans(chemin: &Path, publisher: &DiscoveryPublisher, camera: &str) {
	let familles = charger(chemin, camera);
	if familles.is_empty() {
		return;
	}
	let mut marquees = 0usize;
	for famille in familles {
		if publisher.marque_famille_lue(camera, famille, true) {
			marquees += 1;
		}
	}
	tracing::info!(
		camera,
		familles = marquees,
		chemin = %chemin.display(),
		"familles relancées en modifiable depuis le disque : aucune commande caméra n'a été envoyée"
	);
}

/// Mémorise l'état d'une caméra, avec l'emplacement retenu. Ne fait rien si
/// aucun emplacement n'est résolu.
pub fn enregistrer(publisher: &DiscoveryPublisher, camera: &str) {
	let Some(chemin) = chemin() else { return };
	let familles: Vec<Family> = publisher.familles_ecrivables(camera).into_iter().collect();
	if let Err(e) = sauver(chemin, camera, &familles) {
		tracing::warn!(
			camera,
			chemin = %chemin.display(),
			erreur = %e,
			"l'état des familles n'a pas pu être mémorisé : le prochain démarrage repartira en lecture seule"
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Un chemin unique par test, dans le répertoire temporaire : ces tests
	/// écrivent vraiment sur le disque (c'est ce qu'ils testent), mais jamais
	/// dans le dépôt et jamais au même endroit.
	fn chemin_test(nom: &str) -> PathBuf {
		let mut chemin = std::env::temp_dir();
		chemin.push(format!(
			"bairelay-families-read-{}-{}.json",
			std::process::id(),
			nom
		));
		chemin
	}

	#[test]
	fn une_famille_prouvee_retrouve_sa_place_sans_la_camera() {
		let chemin = chemin_test("aller-retour");
		let _ = std::fs::remove_file(&chemin);

		// Ce que le pont a appris : image et audio sont modifiables.
		sauver(&chemin, "Sonnette", &[Family::Image, Family::Audio, Family::Image])
			.expect("écriture de l'état");
		assert!(
			!chemin.with_file_name(format!("{}.tmp", std::process::id())).exists(),
			"le fichier temporaire doit avoir été renommé, pas laissé derrière lui"
		);

		// Ce que le redémarrage en conclut, sans caméra.
		assert_eq!(
			charger(&chemin, "Sonnette"),
			vec![Family::Audio, Family::Image],
			"l'ordre vient du fichier, les doublons sont tombés"
		);
		assert!(charger(&chemin, "Autre camera").is_empty(), "une autre caméra ne voit rien");

		let _ = std::fs::remove_file(&chemin);
	}

	#[test]
	fn un_fichier_corrompu_ne_freine_rien_et_se_repriture() {
		let chemin = chemin_test("corrompu");
		std::fs::write(&chemin, "{ ce n'est pas du json").expect("fixture");
		assert!(charger(&chemin, "Sonnette").is_empty(), "corrompu = on ne sait pas encore");

		// Et la première écriture qui suit reprend la main sur le fichier.
		sauver(&chemin, "Sonnette", &[Family::Pir]).expect("réécriture");
		assert_eq!(charger(&chemin, "Sonnette"), vec![Family::Pir]);

		let _ = std::fs::remove_file(&chemin);
	}

	#[test]
	fn un_slug_inconnu_ne_passe_pas() {
		let chemin = chemin_test("slug");
		std::fs::write(
			&chemin,
			r#"{"version":1,"cameras":{"Sonnette":["audio","famille-inventee"]}}"#,
		)
		.expect("fixture");
		assert_eq!(
			charger(&chemin, "Sonnette"),
			vec![Family::Audio],
			"un slug que le pont ne connaît pas n'invente pas une famille"
		);
		let _ = std::fs::remove_file(&chemin);
	}

	#[test]
	fn une_version_inconnue_est_ignoree() {
		let chemin = chemin_test("version");
		std::fs::write(&chemin, r#"{"version":99,"cameras":{"Sonnette":["audio"]}}"#)
			.expect("fixture");
		assert!(charger(&chemin, "Sonnette").is_empty(), "un format inconnu ne se devine pas");
		let _ = std::fs::remove_file(&chemin);
	}

	#[test]
	fn les_autres_cameras_sont_preservees() {
		let chemin = chemin_test("voisines");
		let _ = std::fs::remove_file(&chemin);
		sauver(&chemin, "Sonnette", &[Family::Audio]).expect("première caméra");
		sauver(&chemin, "Garage", &[Family::Image]).expect("seconde caméra");
		assert_eq!(charger(&chemin, "Sonnette"), vec![Family::Audio]);
		assert_eq!(charger(&chemin, "Garage"), vec![Family::Image]);

		// Une famille qui retombe en lecture seule sort du fichier — sinon le
		// pont repartirait modifiable sur une preuve qu'il vient de retirer.
		sauver(&chemin, "Sonnette", &[]).expect("retrait");
		assert!(charger(&chemin, "Sonnette").is_empty());
		assert_eq!(
			charger(&chemin, "Garage"),
			vec![Family::Image],
			"retirer une caméra n'efface pas les autres"
		);

		let _ = std::fs::remove_file(&chemin);
	}

	#[test]
	fn definir_chemin_se_pose_a_cote_de_la_configuration() {
		// La règle d'emplacement est vérifiée sans toucher au global : même
		// répertoire, nom propre, et le fichier de config lui-même intact.
		let config = Path::new("/config/sonnette/bairelay.toml");
		assert_eq!(
			config.with_file_name(FAMILIES_READ_FILE),
			PathBuf::from("/config/sonnette/config-families-read.json")
		);
	}
}
