# R1: relay can redirect traffic for a node to another node

Status: **open, not fixed.** This document tracks the issue until a fix is merged.
It describes the problem for maintainers; it is not a fix.

## Summary

When a client sends to node `V` for the first time, it asks the relay for `V`'s node
info and connects to the *default identity in the relay's answer*. Nothing checks
that this default identity is `V`, that `V` is in the answer at all, or that `V`
ever agreed to be an alias of that default identity. A relay that answers
"`V` is an alias of `A`" makes the client open the channel to `A`, and the data the
client meant for `V` is delivered to `A` and decrypted there.

With `encryption-strict`, the point of session encryption is that the relay cannot
read traffic between nodes. R1 bypasses that: the relay cannot decrypt anything
itself, but it can steer the traffic to a node it controls (`A`), which can.

### Who can do this

- The operator of the relay the client uses (a malicious or compromised relay), together
  with any node `A` that the attacker runs and registers on that relay.
- Possibly anyone who can modify packets between the client and the relay. The client
  does not authenticate the relay (see [Root cause](#root-cause)). This part is from
  code reading only and has not been tested on a real network.

### Impact

- Traffic the client sends to `V` over reliable channels (`forward_reliable`, the
  channel yagna uses for GSB requests, with caller, address and payload) goes to `A`.
  The transfer channel (`forward_transfer`) uses the same code path.
- Unreliable traffic (`forward_unreliable`) to `V` is also redirected when the session
  to `A` goes through the relay. This was not part of the original report; it was
  found while testing (see [Verification](#how-this-was-verified)).
- An existing channel to `V` is used as long as it works, but the relay can break it
  (for example with a `Disconnected` notification), and the reconnect goes through the
  same lookup and lands on `A`.

### What the attacker cannot do

- It cannot impersonate `V` in a session handshake or forge `V`'s signatures. The
  session the client opens is a real, authenticated session with `A`.
- In yagna strict builds, `A` cannot answer *as* `V`: yagna's `handle_reply`
  (`core/net/src/hybrid/service.rs`, `authorize_published_identity_with`) rejects a reply
  for a request sent to `V` when `V` is not among the identities authenticated for
  the session it came from. So in strict builds `A` can read requests meant for
  `V`, but its replies are dropped and the caller gets no answer. This is from code
  reading of yagna `release/v0.18`; it was not tested end to end.
- In strict builds, an answer that omits the session key cannot be used to route
  through the relay: the relayed route takes its key from the answer, and strict
  mode refuses a route without one. It *can* be used when `A` is reachable directly
  (see below).

## Affected versions

- **ya-relay client**: `main` at `5a96b47` and earlier. The lookup-and-connect logic in
  `forward_virtual_tcp` dates from 2023 and is in every released client tag, but this
  document was verified only against `main` at `5a96b47` (which is also yagna's pin, below).
- **yagna**: `release/v0.18` pins `ya-relay-client` at `7cbe72a`, whose `client/`,
  `server/` and `crates/` trees are identical to ya-relay `main` at `5a96b47`
  (`git diff --quiet 7cbe72a 5a96b47 -- client server crates`). yagna does no check of
  its own before sending a request (`State::forward_sink` calls `forward_reliable` /
  `forward_unreliable` / `forward_transfer` directly). Affected:
  - `v0.18.0`, `v0.18.1`: default feature `encryption` (not strict).
  - `pre-rel-v0.18.2-rc1` to `pre-rel-v0.18.2-rc4`: default feature `encryption-strict`.

### Build modes

| Build | Effect of R1 |
|---|---|
| `encryption-strict` | Main concern. Relayed traffic is supposed to be unreadable by the relay; R1 lets the relay route it to `A`, which reads it. `A` cannot reply as `V` (yagna strict reply check). |
| `encryption` (not strict) | R1 also reproduces. Encryption is weaker by design here: a route without a session key falls back to plaintext, so the relay has other ways to weaken it, and yagna's non-strict reply check falls back to the relay-published identity list, so (from code reading, not tested) `A` may also be able to reply as `V`. |
| no `encryption` feature | Relayed traffic is plaintext, and the relay can already read it. R1 additionally moves traffic that would have gone peer-to-peer. |

## Root cause

All references are to ya-relay `main` at `5a96b47`.

1. **The reliable channel is opened to the relay's default identity, not to the
   requested one.** `TransportLayer::forward_virtual_tcp`
   (`client/src/transport.rs:198`) calls `session_layer.query_node_info(node_id)`
   (`client/src/transport.rs:215`), then reuses a channel to `info.default_node_id()`
   if one exists (`client/src/transport.rs:217-218`) or connects to it
   (`client/src/transport.rs:230`), and stores the resulting sender under the requested
   `node_id` (`client/src/transport.rs:237`). The existing
   `TODO` at `client/src/transport.rs:214` already suggests going through
   `session_layer.session` instead.

2. **The answer is not checked against the request.** `SessionLayer::query_node_info`
   (`client/src/session.rs:820`) converts the relay answer
   (`client/src/session.rs:843`) and writes it to the network view
   (`client/src/session.rs:846`). It never checks that the requested `node_id` is in
   `info.identities`. `NetworkView::update_entry`
   (`client/src/session/network_view.rs:162-163`) then maps every listed identity to the
   entry of the listed default identity. `SessionLayer::session_filtered_connection_methods`
   likewise takes `info.identities[0]` as the node to connect to
   (`client/src/session.rs:958`).

3. **Alias listings do not have to be proven, and the proof would not be enough.**
   `NodeInfo::try_from(proto::response::Node)` (`crates/core/src/server_session.rs:243`):
   - If `session_pub_key` is empty, no proof is checked at all and the answer is accepted
     with `session_key = None` and no authenticated identities
     (`crates/core/src/server_session.rs:255`, `:294`).
   - If a key is present, only the default identity must have a proof
     (`crates/core/src/server_session.rs:286`). Per-identity proofs may be absent (legacy
     form) or empty (`crates/core/src/server_session.rs:276`). Identities without a proof
     are still listed in `identities` and still used for routing; they are only left out of
     `authenticated_identities`.
   - Even a complete set of proofs does not show that identities belong together. A proof
     is a signature over `H("golem session key" || key)` (`crates/core/src/challenge.rs:101`,
     verified at `crates/core/src/challenge.rs:197`). It shows that an identity delegated
     to a key. `A` can sign `V`'s published session key, and the relay can present
     `[A, V]` with `V`'s key and two valid proofs. Nothing ties the identity list or the
     default identity to the signatures.

4. **The route uses whatever the answer says.** For a direct session the client
   handshakes with `A` and gets `A`'s own session key, so the traffic is encrypted to
   `A`. For a relayed session `SessionLayer::try_relayed_connection` takes the identity
   list and session key from the cached answer (`client/src/session.rs:1372`,
   `:1385`), so the route registered for `V` points to `A`'s slot with `A`'s key.

5. **The relay is not authenticated.** The relay session carries no identities and no
   key; the client recognises it only by that (`is_relay = identities.is_empty()`,
   `client/src/session.rs:224`). Relay responses are plain protobuf over UDP. This is why
   an on-path attacker may be able to act as the relay. Code reading only.

## How this was verified

- **Code reading** of the paths above on ya-relay `main` (`5a96b47`) and of yagna
  `release/v0.18` (pin, default features, `forward_sink`, `handle_reply` /
  `authorize_published_identity_with`).
- **Tests on `main`.** Five attack tests from an unpublished prototype fix branch were
  copied, unchanged in their assertions, into a scratch test file on `main` (not part
  of this PR), together with three variants written for this check (the two keyless
  rows and the last row below), and run with `--features encryption-strict` and again with
  `--features encryption`. They use a UDP proxy between the client and a test relay that
  forwards all traffic and rewrites only the node-info answer about `V`. Each test
  asserts that the attacker node receives nothing. Results on `main` (same in both
  modes):

  | Scenario | Result on `main` |
  |---|---|
  | `[A, V]` with `V`'s key and valid proofs from both (`A` signed `V`'s key), reliable channel | attacker received the data |
  | same, after the relay breaks an existing working channel and it reconnects | reconnect went to the attacker |
  | `V` listed under `A` without a proof, `A` reachable directly, reliable | attacker received the data |
  | same, `A` only reachable through the relay, reliable | attacker received the data |
  | same, through the relay, unreliable | attacker received the data |
  | answer without a session key, `A` reachable directly, reliable | attacker received the data |
  | answer without a session key, through the relay, reliable | not redirected (strict: the client refuses a relayed route without a key; non-strict: the client sends plaintext and `A`, which has an encrypted route to the client, drops it) |
  | `A`'s own unmodified answer returned for a query about `V` (`V` not listed), reliable | attacker received the data |

### Not shown

- No attack was run against a running yagna node, and no GSB request was captured end to
  end. That yagna sends GSB requests over these channels and that strict yagna rejects
  `A`'s replies is from code reading.
- The on-path variant (someone between the client and an honest relay) is from code
  reading only. The test proxy shows that the client accepts rewritten answers from
  whatever answers on the relay address, not that this is reachable on a real network.
- That `A` can reply as `V` in non-strict yagna builds is from code reading only.
- The transfer channel shares the reliable code path but was not tested separately.
  Unreliable traffic over a *direct* session to `A` was not tested.

## Fix direction

Short term (client only, no protocol change):

- Open reliable and transfer channels through `SessionLayer::session` for the
  *requested* identity, instead of connecting to the relay's default identity
  (the existing `TODO` at `client/src/transport.rs:214`).
- Tie traffic for `V` to a session key that `V` itself proved: only route to `V` over a
  session whose key carries a valid proof by `V`, and treat a later change of that key
  as a new route that needs the proof again.
- Require the requested identity to be in the relay answer, and reject the answer
  otherwise.
- In strict builds, fail closed on an answer without a session key instead of skipping
  verification.

Long term (protocol change): have the node sign one statement that binds its session key,
its default identity and its full identity list, signed by each identity, so that a relay
cannot recombine valid proofs into a different grouping. This needs a relay and client
protocol change and a compatibility plan.

A prototype of the short-term fix, with the tests above, exists on a branch that has not
been published yet.

## Related

- N1 (inner source address check in `TcpLayer::dispatch`) is a separate issue and is
  tracked separately.
