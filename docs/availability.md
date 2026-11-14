# Availability

Two independent questions get two independent MQTT blocks, because Home
Assistant can only act on what it can tell apart:

- *is the bridge itself up?* → `{prefix}/status`
- *can this camera be talked to?* → `{prefix}/{cam}/presence`

Every discovered entity of this bridge carries both in its `availability` list
(`DiscoveryAvailability`, with the `visibility_topic` each block needs to be
readable from an automation), so a device goes unavailable with its bridge and
with its camera, without a single extra rule in Home Assistant.

## The bridge block

`{prefix}/status` carries `online` while the bridge is connected to the broker.

The word is published **retained** on CONNACK, and `offline` is registered as the
client's **last will** before the connection is even attempted. That ordering is
the whole point: a crash, a killed container, a pulled cable or a Home Assistant
update leaves the broker to publish `offline` by itself. Nothing has to be
running for the bridge to be marked unavailable — which is the only case where
knowing it matters.

## The device block

`{prefix}/{cam}/presence` carries `reachable` or `unreachable`, retained.

**Presence does not follow the session.** A battery doorbell closes its Baichuan
session a few seconds after every wake, by design: the wake lock is released, the
idle timer runs out, the radio goes back to sleep. Mirroring that on availability
would grey out the whole dashboard for most of every day, and the first thing an
operator does with a permanently-grey dashboard is stop trusting it. Sleeping is
not being gone.

Presence is therefore derived from three events that already happen, and adds
**no network call of its own**:

- the bridge starts → `reachable`, optimistically: the camera was configured, and
  the first thing the bridge does is try it;
- a session is opened → `reachable` (a real contact, not a hope);
- a connection attempt **gives up** → `unreachable`.

## Why one failed attempt is enough

A connection attempt to a sleeping camera is not a cheap probe: it is a radio
wake-up. The camera pays for it in battery whether it answers or not, and the
second attempt buys no information the first one did not already give — the same
camera, the same sleep state, the same radio. What a retry would cost is a second
wake for the same answer.

Retrying is not lost either: the wake machinery already runs on its own clock
(the wake lock, the burst guard, the alarm listener), so if something wakes the
camera for a real reason, the next successful session flips presence back to
`reachable` by itself. Presence is a statement about what the bridge has seen,
not a health check the bridge invents.

That is also why publishes are gated on **transitions**: the latch is shared per
camera (`PublishedPresence`, one cell per camera, not per publisher — which is
why only `StatusPublisher::with_presence` may publish presence), and a verdict
that repeats what is already on the wire is dropped before it reaches the broker.
A retained publish is replayed to every subscriber that comes back, so a
duplicated verdict would make every restarting Home Assistant re-fire its
automations for no news.

## What this is for

An automation that stops working silently is worse than one that does not work.
With these two blocks, "no doorbell event for a day" can be told apart:

| bridge | camera | what it means |
| --- | --- | --- |
| `online` | `reachable` | normal |
| `online` | `unreachable` | the camera stopped answering: battery, Wi-Fi, or it was moved |
| `offline` | (last value) | the bridge is down: the camera may be perfectly fine |
| missing | missing | the broker or the add-on is not running at all |
