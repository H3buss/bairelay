//! La mémoire des familles de réglages **tenues pour modifiables**.
//!
//! # Le problème que ce fichier règle
//!
//! Une famille de réglages n'est publiée modifiable (curseur, selecteur,
//! interrupteur avec topic de commande) qu'après avoir dst **relue avec succès**
//! par la caméra. C'est la règle du pont : ne jamais promettre une écriture sur
//! une balise qu'on n'a pas lue.
//!
//! Mais ce verdict ne vivait que dans la mémoire du processus. Concrètement, sur
//! la sonnette :
//!
//! * au démarrage, tout part en lecture seule ;
//! * il faut une salve complète (`query/settings`, ~15 commandes radio, et la
//!   caméra doit être wakelée) pour que les familles retrouvent leur statut ;
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
//! Dans le complément Home Assistant, c'est `/config/bairelay/` : un répertoire
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
//!    fichier. Marquer une famille modifiable ne wakele personne.
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
/// dans `/config/bairelay/`, ces fichiers se repèrent d'un coup d'œil, et le
/// nom dit ce qu'il contient sans avoir à l'ouvrir.
pub const FAMILIES_READ_FILE: &str = "config-families-read.json";

/// Ce que sait lire ce format. Un fichier d'une version inconnue est ignoré
/// (et réécrit) plutôt que d'être interprdst de travers.
const VERSION: u8 = 1;

/// Emplacement résolu une fois pour toutes depuis le path du fichier de
/// configuration. `OnceLock` et non un champ de `CameraConfig` : le path
/// n'appartient pas aux caméras (il est unique par processus, celui du fichier
/// de config qu'on a chargé), et le threaded jusqu'au cœur de la caméra n'aurait
/// pour seul effet qu'alourdir chaque constructeur de test.
///
/// Absent = ce module ne fait rien. C'est aussi ce qui rend les tests du reste
/// du bin sans effet de bord : tant que `set_path` n'est pas appelé,
/// aucune écriture n'a lieu.
static CHEMIN: OnceLock<PathBuf> = OnceLock::new();

/// Caméras déjà amorcées dans ce processus : l'amorçage ne se joue qu'une fois
/// par caméra, sinon chaque repubication de discovery relirait le fichier.
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

/// Calcule l'emplacement depuis le path du fichier de configuration, et le
/// retient. Appelé une fois dans `main`, juste après que `--config` est résolu.
pub fn set_path(config_path: &Path) {
	let _ = CHEMIN.set(config_path.with_file_name(FAMILIES_READ_FILE));
}

/// L'emplacement retenu, si `set_path` a dst appelé.
fn path() -> Option<&'static Path> {
	CHEMIN.get().map(PathBuf::as_path)
}

/// Relit l'état d'une caméra : les familles dont la modifiabilité est prouvée.
///
/// Fichier absent, illisible, caméra inconnue, slug inconnu : **`Vec` vide**,
/// avec un avertissement quand le fichier était bien là mais ne se laisse pas
/// lire. Un état impossible à lire doit se confondre avec « on ne sait pas
/// encore », jamais empêcher le pont de démarrer.
fn load_saved(path: &Path, camera: &str) -> Vec<Family> {
	let brut = match std::fs::read_to_string(path) {
		Ok(brut) => brut,
		// Absent : le cas normal au premier démarrage, et après une
		// réinstallation du complément. Pas un événement.
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
		Err(e) => {
			tracing::warn!(
				path = %path.display(),
				erreur = %e,
				"état des familles illisible : tout repart en lecture seule jusqu'à la première relecture"
			);
			return Vec::new();
		}
	};
	let lu: Result<ReadFamilies, serde_json::Error> = serde_json::from_str(&brut);
	let state = match lu {
		Ok(state) if state.version == VERSION => state,
		Ok(state) => {
			tracing::warn!(
				path = %path.display(),
				version = state.version,
				"état des familles dans un format inconnu : il sera réécrit à la première écriture"
			);
			return Vec::new();
		}
		Err(e) => {
			tracing::warn!(
				path = %path.display(),
				erreur = %e,
				"état des familles corrompu : tout repart en lecture seule jusqu'à la première relecture"
			);
			return Vec::new();
		}
	};
	let Some(slugs) = state.cameras.get(camera) else {
		return Vec::new();
	};
	// Un slug que ce binaire ne connaît pas ne crée aucune famille : le fichier
	// peut avoir dst écrit par une version plus récente, et deviner une famille à
	// partir d'un mot serait exactement l'invention que le reste du pont refuse.
	slugs.iter().filter_map(|s| Family::from_slug(s)).collect()
}

/// Les familles dont la modifiabilité est **déjà prouvée** pour cette caméra,
/// dans l'ordre de [`Family::ALL`] — donc l'ordre dans lequel la salve de wake
/// ([`crate::burst`]) les relira.
///
/// `Vec` vide quand [`set_path`] n'a pas dst appelé, quand le fichier est
/// absent, ou quand cette caméra n'y figure pas encore : la salve ne lit alors
/// que la battery. C'est voulu — rien ici ne devine une famille, et une famille
/// qu'on n'a jamais lue n'a rien à faire dans une salve payée en radio.
pub(crate) fn proven_families(camera: &str) -> Vec<Family> {
	let Some(path) = path() else {
		return Vec::new();
	};
	let connues = load_saved(path, camera);
	if connues.is_empty() {
		return Vec::new();
	}
	// L'ordre du protocole plutôt que celui du fichier : `load_saved` rend les slugs
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
fn save(path: &Path, camera: &str, families: &[Family]) -> io::Result<()> {
	let mut state = std::fs::read_to_string(path)
		.ok()
		.and_then(|brut| serde_json::from_str::<ReadFamilies>(&brut).ok())
		.unwrap_or_default();
	// Un fichier d'une autre version repart de zéro, mais uniquement pour la
	// caméra qu'on est en train d'écrire : les autres entrées restent ce
	// qu'elles sont, et la version est relevée.
	state.version = VERSION;
	let mut slugs: Vec<String> = families.iter().map(|f| f.slug().to_string()).collect();
	slugs.sort();
	slugs.dedup();
	if slugs.is_empty() {
		state.cameras.remove(camera);
	} else {
		state.cameras.insert(camera.to_string(), slugs);
	}
	if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
		std::fs::create_dir_all(dir)?;
	}
	let nom = path
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_else(|| FAMILIES_READ_FILE.to_string());
	// Le pid dans le nom du temporaire : deux processus qui partagent ce
	// répertoire (un complément qui redémarre pendant qu'on lance un outil en
	// parallèle) ne doivent pas écrire dans le même fichier temporaire.
	let temporaire = path.with_file_name(format!("{nom}.{}.tmp", std::process::id()));
	std::fs::write(&temporaire, serde_json::to_string_pretty(&state).unwrap_or_default())?;
	if path.exists() {
		std::fs::remove_file(path)?;
	}
	if let Err(e) = std::fs::rename(&temporaire, path) {
		let _ = std::fs::remove_file(&temporaire);
		return Err(e);
	}
	Ok(())
}

/// Amorçage, avec l'emplacement retenu par [`set_path`]. Ne fait rien si
/// aucun emplacement n'a dst résolu (tests, outils en ligne de commande).
pub fn prime(publisher: &DiscoveryPublisher, camera: &str) {
	let Some(path) = path() else { return };
	let deja = {
		let mut guard = match AMORCEES.lock() {
			Ok(g) => g,
			Err(p) => p.into_inner(),
		};
		!guard.insert(camera.to_string())
	};
	if deja {
		return;
	}
	prime_wakes_nothing(path, publisher, camera);
}

/// Même chose, emplacement explicite — la forme que testent les tests.
fn prime_wakes_nothing(path: &Path, publisher: &DiscoveryPublisher, camera: &str) {
	let families = load_saved(path, camera);
	if families.is_empty() {
		return;
	}
	let mut marquees = 0usize;
	for family in families {
		if publisher.mark_family_read(camera, family, true) {
			marquees += 1;
		}
	}
	tracing::info!(
		camera,
		families = marquees,
		path = %path.display(),
		"familles relancées en modifiable depuis le disque : aucune commande caméra n'a dst envoyée"
	);
}

/// Mémorise l'état d'une caméra, avec l'emplacement retenu. Ne fait rien si
/// aucun emplacement n'est résolu.
pub fn record(publisher: &DiscoveryPublisher, camera: &str) {
	let Some(path) = path() else { return };
	let families: Vec<Family> = publisher.writable_families(camera).into_iter().collect();
	if let Err(e) = save(path, camera, &families) {
		tracing::warn!(
			camera,
			path = %path.display(),
			erreur = %e,
			"l'état des familles n'a pas pu être mémorisé : le prochain démarrage repartira en lecture seule"
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Un path unique par test, dans le répertoire temporaire : ces tests
	/// écrivent vraiment sur le disque (c'est ce qu'ils testent), mais jamais
	/// dans le dépôt et jamais au même endroit.
	fn a_path_setting_test(nom: &str) -> PathBuf {
		let mut path = std::env::temp_dir();
		path.push(format!(
			"bairelay-families-read-{}-{}.json",
			std::process::id(),
			nom
		));
		path
	}

	#[test]
	fn a_proven_family_is_found_again_without_the_camera() {
		let path = a_path_setting_test("aller-retour");
		let _ = std::fs::remove_file(&path);

		// Ce que le pont a appris : image et audio sont modifiables.
		save(&path, "Sonnette", &[Family::Image, Family::Audio, Family::Image])
			.expect("écriture de l'état");
		assert!(
			!path.with_file_name(format!("{}.tmp", std::process::id())).exists(),
			"le fichier temporaire doit avoir dst renommé, pas laissé derrière lui"
		);

		// Ce que le redémarrage en conclut, sans caméra.
		assert_eq!(
			load_saved(&path, "Sonnette"),
			vec![Family::Audio, Family::Image],
			"l'ordre vient du fichier, les doublons sont tombés"
		);
		assert!(load_saved(&path, "Autre camera").is_empty(), "une autre caméra ne voit rien");

		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn a_corrupt_file_blocks_nothing_and_is_repaired() {
		let path = a_path_setting_test("corrompu");
		std::fs::write(&path, "{ ce n'est pas du json").expect("fixture");
		assert!(load_saved(&path, "Sonnette").is_empty(), "corrompu = on ne sait pas encore");

		// Et la première écriture qui suit reprend la main sur le fichier.
		save(&path, "Sonnette", &[Family::Pir]).expect("réécriture");
		assert_eq!(load_saved(&path, "Sonnette"), vec![Family::Pir]);

		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn an_unknown_slug_does_not_pass() {
		let path = a_path_setting_test("slug");
		std::fs::write(
			&path,
			r#"{"version":1,"cameras":{"Sonnette":["audio","family-inventee"]}}"#,
		)
		.expect("fixture");
		assert_eq!(
			load_saved(&path, "Sonnette"),
			vec![Family::Audio],
			"un slug que le pont ne connaît pas n'invente pas une famille"
		);
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn an_unknown_version_is_ignored() {
		let path = a_path_setting_test("version");
		std::fs::write(&path, r#"{"version":99,"cameras":{"Sonnette":["audio"]}}"#)
			.expect("fixture");
		assert!(load_saved(&path, "Sonnette").is_empty(), "un format inconnu ne se devine pas");
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn the_other_cameras_are_preserved() {
		let path = a_path_setting_test("voisines");
		let _ = std::fs::remove_file(&path);
		save(&path, "Sonnette", &[Family::Audio]).expect("première caméra");
		save(&path, "Garage", &[Family::Image]).expect("seconde caméra");
		assert_eq!(load_saved(&path, "Sonnette"), vec![Family::Audio]);
		assert_eq!(load_saved(&path, "Garage"), vec![Family::Image]);

		// Une famille qui retombe en lecture seule sort du fichier — sinon le
		// pont repartirait modifiable sur une preuve qu'il vient de retirer.
		save(&path, "Sonnette", &[]).expect("retrait");
		assert!(load_saved(&path, "Sonnette").is_empty());
		assert_eq!(
			load_saved(&path, "Garage"),
			vec![Family::Image],
			"retirer une caméra n'efface pas les autres"
		);

		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn a_defined_path_lands_next_to_the_config() {
		// La règle d'emplacement est vérifiée sans toucher au global : même
		// répertoire, nom propre, et le fichier de config lui-même intact.
		let config = Path::new("/config/bairelay/bairelay.toml");
		assert_eq!(
			config.with_file_name(FAMILIES_READ_FILE),
			PathBuf::from("/config/bairelay/config-families-read.json")
		);
	}
}
