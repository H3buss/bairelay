# Camera settings

The settings engine reads and writes the camera's own setting families over the
Baichuan TCP session, mirrors them onto MQTT, and offers them to Home Assistant
as discovered entities. It exists because a battery doorbell answers for a few
seconds at a time: everything here is organised around spending that window
well.

The protocol-level notebooks the measurements come from
(`docs/protocole-reglages.md`, `docs/reglages-app-reolink.md`,
`docs/canal-evenements-udp.md`) live with the capture tooling on the audio
branch. Wherever one of those measurements decides a behaviour, the number is
restated here, so this file is enough to review the behaviour.

## Families

A family is one block of camera settings with its own read/write message ids and
its own topic segment. `Family::slug()` is the topic segment; the same slugs are
what `query/settings` accepts.

| slug | block | read / write | writable |
| --- | --- | --- | --- |
| `audio` | `<audioCfg>` | 264 / 265 | yes |
| `denoise` | `<aiDenoise>` | 439 / 440 | yes |
| `image` | `<VideoInput>` + `<InputAdvanceCfg>` | 26 / 25 | yes |
| `osd` | `<OsdDatetime>` + `<OsdChannelName>` | 44 / 45 | partly (see below) |
| `motion` | `<MD>` / `<MdAlarm>` | 46 / 47 | partly (see below) |
| `pir` | `<RfAlarmCfg>` | 212 / 213 | yes |
| `ntp` | `<Ntp>` | 38 / 39 | yes |
| `dst` | `<Dst>` | 106 / 107 | yes |
| `autoreboot` | `<AutoReboot>` | 101 / 100 | yes |
| `general` | `<SystemGeneral>` | 104 / 105 | yes |
| `chime` | paired chime | 484, 485, 486, 487 read · 483, 485, 486, 610 write | yes |
| `mask` | `<Shelter>` | 52 | no |
| `sleep` | `<sleepState>` | 574 | no |
| `wifi` | `<WifiSignal>` | 115 | no |
| `battery` | `<BatteryInfo>` | 253 | no, and never worth a wake |
| `ai` | `<AiDetectCfg>` | 342 / 343 | pinned read-only (see below) |

### Why some families are read-only

These are not gaps in the UI; each one is a camera behaviour that was measured,
and the bridge publishes a reason next to the entity so the operator is never
left with a silent switch:

- **`motion`** — the getter (46) answers with `<MdAlarm>`, a flat shape the
  setter (47) rejects. Writing it back would be a guaranteed refusal, so the
  bridge refuses locally and says so (`status/motion/reason`). `<MD>` answers
  47 normally, so a camera that gives `<MD>` keeps its switches.
- **`osd`** — the name and the watermark live in `<OsdChannelName>`, which the
  read does not return. A write would have to invent the rest of that block.
- **`mask`, `sleep`, `wifi`** — one-way blocks: the camera prints them and
  accepts no equivalent write (`<Shelter>` has no setter on this firmware).
- **`battery`** — readable, and deliberately never read to *buy* a wake: 253 is
  answered only when the camera is already up, so asking for it first would pay
  a wake-up for a number.
- **`ai`** — the per-type detection settings (342/343) are asked one type at a
  time and the enable tag has never been seen in a 342 dump from this firmware
  generation. The family is pinned read-only, with a reason, rather than
  publishing switches that cannot be told from ones that work.

The verdict of every read is one of four things (`Verdict`): **writable** (the
block came back and we can send it back), **not writable** (structural proof we
cannot send it back — this is the bridge refusing, not the camera), **transient**
(a `400`, a timeout, an unusable block: proves nothing, so the switches stay as
they were and the reason is published next to them), or **unknown** (nothing was
even attempted: no session, no capability — discovery changes nothing, the reason
is still published).

That distinction is the point of the design. A sleeping firmware answers `400`
to video blocks and gives them back twenty seconds later; treating that as "not
writable" would strip working controls from Home Assistant until the next
restart.

## Topics

| topic | direction | meaning |
| --- | --- | --- |
| `{prefix}/{cam}/control/{family}/{leaf}` | in | one value, as the camera would print it |
| `{prefix}/{cam}/status/{family}/{leaf}` | out | the value as read back |
| `{prefix}/{cam}/status/{family}/json` | out | the whole block, camera spellings |
| `{prefix}/{cam}/status/{family}/reason` | out | why a family is read-only, or why a read failed |
| `{prefix}/{cam}/query/settings` | in | re-read every family (one wake window) |
| `{prefix}/{cam}/query/settings/{family}` | in | re-read one family |
| `{prefix}/{cam}/status/burst` | out | what the last read burst did, as JSON |

A control message is validated **before** anything is sent: an out-of-range
volume, an unknown leaf, a text longer than the camera accepts, an NTP server
that is not a host, are all refused on the spot, and the refusal is answered with
the reason instead of silence. `FAIL` on the command topic means the camera said
no; a local refusal never reaches it.

## The read burst

`src/burst.rs`. A doorbell press or a motion edge is the only moment a sleeping
camera is reachable, so the reads are scheduled into that window rather than
asked whenever someone wants an answer.

Rules, and what each one is for:

- **one burst at a time**, with a `GUARD_WINDOW` of 20 s: a visitor press, the
  webhook push and the alarm event arrive inside the same half-second. Without
  the window the bridge pays two full read rounds for one finger on the button.
- **a budget, not a wish list**: `COMMAND_BUDGET` = 8 commands, `TIME_BUDGET`
  = 25 s. The real limit is the camera — measured `status/burst` runs show four
  of eight commands refused with a session of about fifteen seconds — so the
  plan is ordered by what the camera answers in time, not by the catalogue:
  `motion` and `osd` first (the two families that answer every day, in the frame
  shape this firmware accepts), then the rest cheapest-first, `image` last (its
  26 slot answers `<Shelter>` instead of `<VideoInput>`, which costs a retry).
- **a family that refuses three wakes in a row drops out of the plan**, and two
  refusals in a row end the burst. The counts live in memory on purpose: the
  durable question ("is this family writable") is the state file's job, while
  "am I still paying for a read this camera keeps refusing" is a per-session
  question, and a restart reopening the three attempts is exactly what a restart
  should mean.
- **battery is asked last**, and only after it has answered enough times to be
  worth the command.
- the burst publishes `status/burst` with the plan, the families read, the ones
  skipped and the reason it stopped.

`Wake` is what started it — `ButtonPress`, `Motion` or `Push` — and appears in
that status payload.

## Which families may be written: the state file

Discovery marks a family's entities writable only when a read has proved it. On
restart the bridge would otherwise have to re-read every family to know that, so
the proven families are kept next to the config file, in
`config-families-read.json`:

```json
{"version":1,"cameras":{"Doorbell":["audio","osd","motion"]}}
```

Slugs only — no values, no identifiers, nothing to protect. A missing,
unreadable or future-version file is treated as "nothing proved yet", which is
the safe direction: entities come back read-only until the next read, and the
bridge never starts refusing to boot because of a scratch file.

## Settings one-shots

For a camera in front of you, without MQTT:

```
bairelay audio-settings -c config.toml Porche --volume 60 --talk 45
bairelay chime-settings -c config.toml Porche --volume 3 --melody 5 --ring
```

Both read first, rewrite only what you named, and print the values the camera
answered afterwards — the camera's answer, not the value you asked for. They are
the renamed `reglages-audio` / `reglages-carillon`; see the migration table.

## Stream selection

`stream` in a camera block (`none | all | main | sub | extern`, default `all`)
decides which RTSP streams **that camera may serve**. The URL suffix picks what
a client *asks for*: `/cam`, `/cam/main`, `/cam/sub`, `/cam/extern`
(case-insensitive; `mainstream`, `substream`, `externstream` also parse),
resolved in `crates/rtsp/src/url.rs` and used at
`crates/rtsp/src/server/connection.rs`. The option and the suffix are two
different questions, and the option is the one that used to be ignored: a camera
set to `stream = "sub"` woke its radio and served the main stream to anyone who
typed `/main`.

- `all` (default) answers every stream — nothing changes for a configuration
  that never mentioned the option;
- `main`, `sub`, `extern` answer that stream only;
- `none` answers none, for a camera that should never be watched over RTSP;
- a refusal happens **before the wake lock is taken**: a request the camera will
  not serve cannot cost a sleeping camera a wake-up, nor hold a reference that
  keeps it awake while it is being refused.

`extern` is a request for a balanced stream, and the RTSP server retries an
unavailable `extern` as `sub`. So with `stream = "sub"` a `/extern` client is
served the substream through that retry — the refusal is what routes it there.
With `stream = "main"` both halves are refused and the client gets the error,
naming the option both times.

On a battery doorbell this is a battery option, not a picture-quality option: the
main stream is a radio wake-up, a full-bitrate session and a held wake lock. A
setup that only ever shows the substream in Home Assistant should say
`stream = "sub"`, and then a stray `/main` from an old dashboard costs nothing
instead of costing a wake.

## Renaming

Everything the bridge **shows** is English now: Home Assistant entity names, the
reason sentences on `status/{family}/reason`, and the values on `status/burst`.
Everything Home Assistant **keys on** is untouched: `unique_id`, `object_id`, the
config topics, the state topics and the state payloads.

That distinction is the whole migration. Home Assistant assigns an `entity_id`
once, when it first sees a discovery record, and never renames it when the
`name` field later changes. So his automations that reference
`number.bairelay_frontdoor_volume_visiteur` keep working exactly as written; what
changes is the label next to it in the dashboard. The two things that would have
broken them — a changed `unique_id`, or a retained config left behind on an old
topic — did not happen here, and `topic_names_match_spec` plus
`chaque_spec_a_un_unique_id_unique` are what keep them from happening later.

### Entity names

| was | now |
| --- | --- |
| Volume général | Speaker volume |
| Volume interphone (parole) | Intercom volume (talk) |
| Volume visiteur | Visitor volume |
| Haut-parleur visiteur | Visitor loudspeaker |
| Débruitage micro | Microphone denoise |
| Force débruitage | Force denoise |
| Luminosité | Brightness |
| Contraste | Contrast |
| Saturation | Saturation |
| Teinte | Hue |
| Netteté | Sharpness |
| Mode jour/nuit | Day/night mode |
| Horodatage à l'écran | On-screen timestamp |
| Nom à l'écran | On-screen name |
| Texte du nom à l'écran | On-screen name text |
| Filigrane | Watermark |
| Détection de mouvement | Motion detection |
| Sensibilité mouvement | Motion sensitivity |
| Sensibilité PIR | PIR sensitivity |
| Intervalle entre deux déclenchements PIR | PIR trigger interval |
| Synchro NTP | NTP sync |
| Serveur NTP | NTP server |
| Intervalle de synchro | Sync interval |
| Heure d'été | Daylight saving |
| Décalage heure d'été | Daylight saving offset |
| Redémarrage automatique | Auto reboot |
| Jour de redémarrage | Reboot day |
| Heure de redémarrage | Reboot hour |
| Minute de redémarrage | Reboot minute |
| Format de date | Date format |
| Format heure (0=24h, 1=12h) | Time format (0=24h, 1=12h) |
| Langue | Language |
| Nom système de l'appareil | Device system name |
| En heure d'été actuellement | Currently in daylight saving |
| Masquage actif | Masking active |
| Emplacements de masque | Mask slots |
| En veille | Sleeping |
| Signal Wi-Fi | Wi-Fi signal |
| Tension batterie | Battery voltage |
| Courant batterie | Battery current |
| Température batterie | Battery temperature |
| État de charge | Charge status |
| Batterie faible | Battery low |
| Carillons appairés | Paired chimes |
| Volume du carillon | Chime volume |
| Mélodie du carillon | Chime melody |
| Mélodies déclarées par la caméra | Camera-declared melodies |
| Faire sonner le carillon | Ring the chime |
| Horaires du projecteur | Floodlight schedule |
| Mouvement détecté | Motion detected |
| Appui sur le bouton | Button press |
| Voyant lumineux | Status light |
| Mode infrarouge | Infrared mode |
| Détection PIR | PIR detection |
| Niveau de batterie | Battery level |
| PTZ gauche / droite / haut / bas | PTZ Left / PTZ Right / PTZ Up / PTZ Down |
| Reconnaissance personne / véhicule / animal / figure / colis | Person detection / Vehicle detection / Animal detection / Face detection / Package detection |

### Reason sentences on `status/{family}/reason`

Non-retained, ephemeral, nothing keys on them — they are for the human reading
the log or the entity's last state.

| was | now |
| --- | --- |
| la caméra n'a pas répondu à temps | the camera did not answer in time |
| la caméra lit ce réglage mais refuse qu'on lui renvoie le bloc : lecture seule | the camera reads this setting but refuses the block back: read-only |
| aucune session Baïchuan en cours : rien n'a été envoyé à la caméra | no Baichuan session is open: nothing was sent to the camera |
| — réglage non verrouillé : la famille garde son état | — setting not locked: the family keeps its state |
| erreur de lecture transitoire : la famille reste publiée modifiable | transient read error: the family stays published as writable |
| la réponse de la caméra ne contient pas `<OsdChannelName>` : rien à lui renvoyer, lecture seule | the camera's answer carries no `<OsdChannelName>`: nothing to send back, read-only |
| 484 a répondu sans aucune entrée reconnue : carillon non énuméré, … | 484 answered with nothing recognised: chime not enumerated, … |
| la caméra n'a pas répondu à 484 : aucun identifiant de carillon, … | the camera did not answer 484: no chime id, … |
| aucune balise d'activation connue par type (342/AiDetectCfg ne documente que … | no known enable tag for this type (342/AiDetectCfg documents only … |
| la caméra n'a donné aucun motif | the camera gave no reason |
| motif masqué : il aurait pu contenir un secret | reason hidden: it could have contained a secret |

The credential mask still matches the French words `mot de passe` — that list is
a scanner over the camera's text, not a label, and translating it would blind the
mask.

### `status/burst` payload

The keys changed too, because the topic is new on this branch and nothing
consumes it yet — cheaper to fix now than after someone builds a dashboard on it.

```json
{"raison":"appui","etat":"terminee","commandes":3,"budget":8,"familles":2}
{"reason":"press","state":"done","commands":3,"budget":8,"families":2}
```

| key | now |
| --- | --- |
| raison | reason — `appui`→`press`, `mouvement`→`motion`, `poussee`→`push` |
| etat | state — `sans-mqtt`→`no-mqtt`, `flux`→`stream`, `sans-session`→`no-session`, `refus`→`refused`, `budget`→`budget`, `duree`→`duration`, `annulee`→`cancelled`, `terminee`→`done` |
| commandes | commands |
| familles | families |

### Ghost entities, and what still guards against them

When an `object_id` really does change, Home Assistant publishes the new entity
and keeps the old one until an empty retained payload arrives on the old config
topic. The bridge already does that diff: `publish()` records the topic set it
published per camera and clears every topic that disappeared from it
(`publish_diffs_against_previous_set_clears_disappearing_topics`,
`une_famille_relue_se_publie_vide_avant_sa_nouvelle_configuration`), which covers
a *known* entity whose identity changes — not only an unknown camera.

The one thing that mechanism cannot see is a change made **between two runs**:
the recorded set lives in memory, so a restart never diffs against what the
previous version published. Nothing needs that today — no slug moved — but this
pass nearly needed it: the label pass first rewrote `AiObject::Animal =>
"animal"` inside `topic_name()`, which is the topic and `unique_id` suffix, not
the label, and `topic_names_match_spec` failed immediately. So the protection is
the pinned wire contract, not the cleanup: `topic_name()` now carries a doc line
saying it is wire surface, and those tests are what stand between a rename and
two hundred entities his dashboard no longer finds.


## Options

Everything the engine reads, and where the rest of its tuning lives:

| option | default | what it does |
| --- | --- | --- |
| `[[cameras]].stream` | `"all"` | which RTSP streams this camera serves (see above) |
| `[[cameras]].mqtt.<…>` | per upstream | which entities of the camera are published at all |

The burst's numbers are **not** configuration. `GUARD_WINDOW`, `COMMAND_BUDGET`,
`TIME_BUDGET`, `SESSION_WAIT`, `SESSION_POLL`, the three refusal limits and the
three read ranks come from measurements on the owner's camera on 2026-11-13, and
each is named for the measurement it encodes at its definition. A knob that
invites a value nobody has measured is how a tuned path gets untuned; when there
is a second camera to measure against, they become options.

## Migration, French names → English

Renamed on this branch, with the breaking ones called out:

| was | now |
| --- | --- |
| `bairelay reglages-audio` | `bairelay audio-settings` |
| `bairelay reglages-carillon` | `bairelay chime-settings` |
| `--interphone` | `--talk` (writes `<talkAndReplyVolume>`) |
| `--messages` | `--visitor-volume` (writes `<visitorVolume>`) |
| `--visiteur` | `--loudspeaker` (writes `<visitorLoudspeaker>`) |
| `--melodie` | `--melody` |
| `--silencieux` | `--silent` |
| `--sonner` | `--ring` |
| `{prefix}/{cam}/query/reglages[/{family}]` | `…/query/settings[/{family}]` |
| `{prefix}/{cam}/status/evenement` | `…/status/event` |
| `{prefix}/{cam}/status/salve` | `…/status/burst` |
| `config-familles-lues.json` | `config-families-read.json` |
| `[annonces] interroger` / `tenue_secs` / `intervalle_secs` | removed — see below |
| `[carnet] …` | removed — see below |
| french text or a guillemet in a payload | not a mistake: see “Values that stay french, and why” |

The flag names follow the tag each one writes, so `--loudspeaker` is the
visitor loudspeaker switch and `--visitor-volume` is the volume of the pre-set
message clips — the two are easy to confuse from the camera's XML, where both
tags start with `visitor`.

`Config` uses `deny_unknown_fields`, so an old `[annonces]` or `[carnet]` block
is a startup error naming the key. Delete the block: both tables configured
listeners whose modules are on the audio branch, and nothing on this branch read
them. Keeping them renamed would have been an option that looks documented and
governs nothing, which is the bug this branch just fixed in `stream`.

The state file is simply not read any more under its old name: the first start
treats every family as unproven, so entities come back read-only until the next
successful read. That is the safe direction of the mistake — a missing write,
never a wrong one.

## Values that stay french, and why

Some french survives in this tree on purpose, and a reader who finds it should be able
to tell in ten seconds whether they are looking at a bug or at a tripwire. The rule
used on every pass of the english migration, and the list of what it kept:

1. **A tripwire.** Text whose job is to *match* french or a secret, so translating it
   would blind the check that protects the operator.
   - `REASON_INTERDITS` in `crates/mqtt/src/status.rs` contains `mot de passe`
     alongside `password`, `passwd`, `token`, `secret` and `apikey`. A reason that
     carries a credential is replaced by `reason hidden: it could have contained a
     secret` before it reaches `status/{family}/reason`; the french entry is there
     because the camera, the reference library and the operator's own logs are
     partly french. Removing the entry would keep the sentence pretty and let a
     password be retained on the broker.
   - the `FRENCH` word list in `src/published_text_guard.rs` and
     `crates/mqtt/src/published_text_guard.rs`. The guard asserts that everything
     the bridge publishes is ASCII and contains none of those words, so its own
     source has to name them. Homographs are excluded on purpose — `volume`,
     `budget`, `part`, `port`, `tour`, `camera`, `status` are french words too, and
     putting them in the list makes the guard fail on `Speaker volume`.
2. **A capture.** A measurement that quotes what the broker really carried, usually
   the value published by the version *before* the switch. Deleting it deletes the
   reason the guard exists.
   - `src/burst.rs`, the 13 November `status/burst` measurement: the camera answered
     `etat:"refus"` at four commands out of eight, both for `raison:"appui"` and for
     `raison:"mouvement"`. Those are payload bytes from the old build, not values the
     current build can emit (it publishes `state:"refused"` and `reason:"press"`).
   - the negative assertion in the same file, `!state.contains("\"state\":\"refus\"")`,
     which only has meaning while it spells the old french value.
3. **Non-ASCII punctuation that is published deliberately.** MQTT payloads are UTF-8
   and Home Assistant renders these without trouble; the only cost is that a
   punctuation control counts them, which is the lesser evil.
   - `«` and `»`. `publishable_reason_bounded` maps `<` to `«` and `>` to `»` so that
     **no raw XML can ever reach a retained topic**: a reason quoting the camera's own
     markup stays readable (`this camera's reply has no «volume» tag, so this setting
     cannot be written…`) without becoming a payload that a consumer might parse. The
     guarantee is asserted in `src/mqtt_dispatch.rs` by a test that refuses a reason
     containing `<`. Replacing the mapping with `[X]` would be ASCII at the price of
     changing a published value *and* rewriting the assertion that guards it, which is
     the class of risk this migration exists to retire.
   - ` […]`, appended by `cut()` in `crates/core/src/bc_protocol/settings.rs` when a
     rendered text is truncated. The truncation is by characters, never by bytes,
     because a camera answer carries UTF-8 and a byte-offset cut in the middle of an
     accent panics — a log line is meant to be read, not to be exhaustive.

Anything else that is french, or above ASCII, in a comment, a log message, a test name
or a published value is a migration bug: fix it where you find it, and if it is a
published value, give it a row in the table above.
