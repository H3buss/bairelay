# Meeting the two-way-audio branch

This file exists because a merge resolution either lives in the repository or it does not
exist in six months. It is written from the two diffs, not from memory: base `9346832`, the
audio branch at `feature/two-way-audio` head `5c39f08`, this branch at its own head. Every
count below was produced by the three commands at the bottom of this file, so you can re-run
them against different heads and get different, honest numbers instead of these ones.

Recompute before trusting any of it:

```sh
base=9346832a70f47f1b4d5c0efa175e259d4888084a
git -C <audio-tree>  diff --name-only "$base"..HEAD | sort > /tmp/a.txt
git -C <this-tree>   diff --name-only h3/main..HEAD | sort > /tmp/m.txt
comm -12 /tmp/a.txt /tmp/m.txt          # the merge surface
```

The surface is **sixteen files**, not fourteen, and the number moves whenever either branch
commits. Count it; do not recall it.

## The sixteen files

`+/-` are lines each branch added and removed in that file, measured against the shared base.

| file | audio | this branch | resolution |
|---|---|---|---|
| `.gitattributes` | 1+/0− | 1+/0− | **identical single line** (`* text=auto eol=lf`) — no conflict, take either |
| `.gitignore` | 11+/0− | 11+/0− | both add ignore blocks for measurement artefacts, 9 lines in common; keep one block with both sets of patterns |
| `README.md` | 56+/3− | 10+/0− | two independent sections; keep both (theirs: two-way audio; ours: settings families) |
| `crates/core/src/bc/xml.rs` | 26+/9− | 1465+/1− | **audio side wins for the three list wrappers** (see below); this branch's ~167 names are additive and re-apply on top |
| `crates/core/src/bc_protocol.rs` | 96+/4− | 101+/6− | union of the module lists and the `pub use` lines — **but six names are added twice** (see below) |
| `docs/implementation.md` | 1+/0− | 37+/0− | no overlap; keep both |
| `sample_config.toml` | 1+/0− | 6+/0− | no overlap; keep both |
| `src/cli.rs` | 221+/2− | 244+/0− | both add subcommands and `common: OneShotArgs` blocks; union, then read the `talk` note below |
| `src/cli_convert.rs` | 40+/0− | 30+/0− | union of the render arms |
| `src/config.rs` | 17+/0− | 17+/1− | **take the audio side alone**: this branch dropped `two_way_audio` (see below) |
| `src/lib.rs` | 1+/0− | 15+/0− | union of `pub mod`: `audio_control`, `burst`, `family_state`, `settings` |
| `src/oneshot/dispatch.rs` | 20+/2− | 11+/3− | union of the match arms over `Outcome`; both remove a line or two of the pre-existing dispatch |
| `src/oneshot/mod.rs` | 5+/0− | 2+/0− | union of `pub mod`: `audio_bridge`, `input`, `pcm`, `talk`, `talk_capability`, `audio_settings`, `chime_settings` |
| `src/oneshot/output.rs` | 369+/0− | 760+/0− | **union of the variant sets, concatenation of the render arms** — the largest textual conflict, and the most mechanical |
| `src/oneshot/runner.rs` | 94+/25− | 84+/25− | 88 added lines are *the same lines*: the runner plumbing was refactored twice. Take one refactor, never both |
| `src/run_support.rs` | 212+/5− | 240+/5− | 44 lines in common, plus two helpers doing the same job under two names (see below) |

## Names both branches add to the same file

A textual union duplicates these, and rustc tells you about some of them and not others. Kept
as a list you can grep the merged tree against:

- `crates/core/src/bc_protocol.rs`: `declare_ability`, `ability_assertions`,
  `declared_ability_does_not_leak_to_other_abilities`,
  `declared_ability_keeps_the_advertised_report_false`,
  `declared_ability_opens_the_gate_without_claiming_advertisement`. Both branches grew the
  same ability-override mechanism, on the same reasoning, with the same tests. Keep one
  implementation; the two test sets assert the same three properties, so one set is enough.
- `src/config.rs`: `two_way_audio`. **This branch no longer has the field** — it read nothing
  here, and its doc comment described a talk path this branch does not build. The field
  belongs to the audio branch, where the code that acts on it lives. If this branch ever needs
  the override, it must arrive with a caller.
- `src/oneshot/runner.rs`: `run_graceful`, `run_with_op_timeout`, `connect_login_logout`,
  `op`, `op_timeout`, `cancel`.
- `src/run_support.rs`: `on_worker`, `result`, `mode`, `run_oneshot_dispatch_path`, plus one
  test name each side wrote independently.
- `src/cli.rs` / `src/cli_convert.rs`: the `common` field and the `else` arms — noise, but they
  are what makes git report these files as conflicting when the real content is disjoint.

## Two helpers, one job

`src/run_support.rs` carries, on the audio side, `exit_code_for_outcome(mode, result, stdout,
stderr) -> i32`, and on this side, `code_for_output` with **the same signature**. Both were
written to answer "which exit code does this outcome get", neither knew about the other, and
nothing will flag them: the compiler sees two well-formed functions. Collapse them to one name
when merging, and keep both branches' tests that an output error stays classified and that a
successful run returns zero.

## `xml.rs`: the list form wins

The audio branch turns `<duplexList>`, `<audioStreamModeList>` and `<audioConfigList>` into
wrappers holding repeated children (`duplex: Vec<String>`, `audio_stream_mode: Vec<String>`,
`audio_config: Vec<AudioConfig>`), because a scalar field makes quick-xml drop the whole
`<TalkAbility>` reply with `duplicate field \`duplex\``. That is a fact about the wire,
measured on hardware, so it wins over any local preference. It does not disturb this branch:
`TalkConfig` keeps its scalar `duplex`, `audio_stream_mode` and `audio_config` (in their tree
`TalkConfig` at `bc/xml.rs:1024` is untouched and the new `Vec` fields live on the list
wrappers at `:1094`, `:1108`, `:1121`), so the upstream assertions that read `tc.duplex` still
hold. This branch's additions — the settings families, the chime pairs, roughly 167 names —
touch none of the three fields.

## The `talk` collision that is not a merge conflict

The audio branch adds a **subcommand** `talk`: send a WAV to the camera's loudspeaker
(`--file`, `--raw`, `--block-length`, `--gain`, `--header`, `--idle`, `--input`). This branch
adds a **flag** `--talk` to the `audio-settings` subcommand, meaning `talkAndReplyVolume`,
0-100: the intercom volume, a setting, not a session.

Before merging they sit at line 308 (their `Talk {`) and line 310 (our
`#[arg(long = "talk", value_name = "N")]`) of their respective `src/cli.rs`. A fast merge will
put them two lines apart in one enum, where they look like the same idea spelled twice. They
are not: one opens a session and paces ADPCM at the firmware, the other writes one XML tag and
reads the block back. Renaming either is a user-visible break, so the resolution is a doc line
on each, in the merged file, naming the other.

## What a textual merge cannot tell you

Two branches, one base, one operator, one doorbell. Several things were therefore built twice
without being the same code: the ability override, the exit-code helper, the runner plumbing,
and two doc comments reciting the same `<TalkAbility>` measurement with different command
names. Compile errors catch the duplicated *names*. Nothing catches two functions computing the
same exit code, nor a paragraph that is true of one branch and merely fluent in the other — that
second class is what a line-by-line read of the merged diff is for.

After merging, run, in this order:

```sh
bash /home/openclaw/controles/controles.sh <merged-tree>
cargo doc --workspace --no-deps --document-private-items   # read every 'unresolved link to'
grep -n 'fn .*mode.*result.*stdout.*stderr' src/run_support.rs   # the two-helpers pattern
```
