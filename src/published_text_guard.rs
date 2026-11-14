//! Nothing in french may ever reach the network again.
//!
//! This test exists because the defect it guards against already happened: nine
//! reason sentences were still french on the wire — `status/<family>/reason` was
//! carrying `la caméra a refusé la commande (code 400)` to Home Assistant — behind a
//! commit message stating that published text had been switched to english. A
//! published french value is not cosmetic: the operator's automations compare those
//! sentences, and when they stop matching they fail for a reason nobody can see.
//! Review counts comment prose, not published values, so nothing caught it; this does,
//! and it does not rot.
//!
//! Scope: the values this crate hands to MQTT — reason sentences, the short words of
//! the burst state, the topic segment of every family. The discovery payload is
//! covered by the sibling guard in `bairelay_mqtt` (its own spec table lives there).
//! Test fixtures and the credential mask's own word list are not walked: the mask
//! looks for `mot de passe` on purpose, so that string is supposed to be french.

#[cfg(test)]
mod tests {
	use crate::burst::{Stop, Wake};
	use crate::settings::{
		REASON_AI_NO_TAG, REASON_CHIME_NO_ENTRY, REASON_CHIME_NO_ID, REASON_NO_LOCK, REASON_NO_OSDCHANNEL,
		REASON_NO_SESSION, REASON_READ_ONLY, REASON_TIMEOUT, REASON_TRANSIENT,
	};
	use bairelay_mqtt::settings::Family;

	/// French words that are not english words. Deliberately small, and deliberately
	/// without homographs: `budget`, `part`, `port`, `tour`, `camera`, `status` and
	/// `state` exist in both languages, and a guard that cries wolf gets silenced
	/// inside a month.
	const FRENCH: &[&str] = &[
		"la", "les", "des", "du", "une", "est", "dans", "pour", "sur", "par", "avec", "sans", "que", "qui", "pas",
		"plus", "tres", "tout", "toute", "autre", "chaque", "comme", "aussi", "donc", "mais", "seul", "seule",
		"caméra", "refusé", "réponse", "reglages", "réglages", "balise", "aucun", "aucune", "rien", "toujours",
		"jamais", "encore", "voici", "veut", "peut", "faire", "dit", "nomme", "publie", "motif", "famille",
		"familles", "lecture", "écriture", "sauf", "touche", "arrive", "restent", "revient",
	];

	/// Asserts one published value: printable ascii, and no french word in it.
	pub(crate) fn published_value_is_english(what: &str, value: &str) {
		assert!(value.is_ascii(), "{what} is not ascii on the wire: {value:?}");
		assert!(
			value.chars().all(|c| c.is_ascii_graphic() || c == ' '),
			"{what} carries a control character: {value:?}"
		);
		let lowered = value.to_lowercase();
			let bad: Vec<&str> = lowered
			.split(|c: char| !c.is_alphabetic() && c != '-')
			.filter(|w| FRENCH.contains(w))
			.collect();
		assert!(bad.is_empty(), "{what} would publish french to Home Assistant: {value:?} contains {bad:?}");
	}

	#[test]
	fn every_reason_sentence_is_english() {
		for (name, value) in [
			("REASON_TIMEOUT", REASON_TIMEOUT),
			("REASON_READ_ONLY", REASON_READ_ONLY),
			("REASON_NO_SESSION", REASON_NO_SESSION),
			("REASON_NO_LOCK", REASON_NO_LOCK),
			("REASON_TRANSIENT", REASON_TRANSIENT),
			("REASON_NO_OSDCHANNEL", REASON_NO_OSDCHANNEL),
			("REASON_CHIME_NO_ENTRY", REASON_CHIME_NO_ENTRY),
			("REASON_CHIME_NO_ID", REASON_CHIME_NO_ID),
			("REASON_AI_NO_TAG", REASON_AI_NO_TAG),
		] {
			published_value_is_english(name, value);
		}
	}

	#[test]
	fn every_burst_state_word_is_english() {
		for wake in [Wake::Press, Wake::Motion, Wake::Push] {
			published_value_is_english("Wake::slug", wake.slug());
		}
		for stop in [
			Stop::NoMqtt,
			Stop::Stream,
			Stop::NoSession,
			Stop::Refused,
			Stop::Budget,
			Stop::Duration,
			Stop::Cancelled,
			Stop::Done,
		] {
			published_value_is_english("Stop::slug", stop.slug());
		}
	}

	#[test]
	fn every_family_topic_segment_is_english() {
		for family in Family::ALL {
			published_value_is_english("Family::slug", family.slug());
		}
	}
}
