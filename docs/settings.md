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
| `{prefix}/{cam}/query/reglages[/{family}]` | `…/query/settings[/{family}]` |
| `{prefix}/{cam}/status/evenement` | `…/status/event` |
| `{prefix}/{cam}/status/salve` | `…/status/burst` |
| `config-familles-lues.json` | `config-families-read.json` |
| `[annonces] interroger` / `tenue_secs` / `intervalle_secs` | removed — see below |
| `[carnet] …` | removed — see below |

`Config` uses `deny_unknown_fields`, so an old `[annonces]` or `[carnet]` block
is a startup error naming the key. Delete the block: both tables configured
listeners whose modules are on the audio branch, and nothing on this branch read
them. Keeping them renamed would have been an option that looks documented and
governs nothing, which is the bug this branch just fixed in `stream`.

The state file is simply not read any more under its old name: the first start
treats every family as unproven, so entities come back read-only until the next
successful read. That is the safe direction of the mistake — a missing write,
never a wrong one.
