# Native MTProto state machines

Scope: the shipped Rust crates `mtproto-core`, `mtproto-engine` and `mtproto-ffi`, from request admission and transport selection to callbacks. The Swift and Objective-C engines, benchmarks, fuzz drivers and fixture servers are consumers or test tools. The audit baseline is `da4b2170210aab0430c4ff18c032991f3030cf45`, the native gitlink in macOS beta `393090efb9`. Names below are source anchors. Keep the transitions and issue evidence current when changing these lifecycles.

## 1. Worker, session and FFI lifetime

[Engine](../crates/mtproto-engine/src/lib.rs), [Worker](../crates/mtproto-engine/src/worker.rs), [runtime](../crates/mtproto-engine/src/session_runtime.rs), [drain](../crates/mtproto-engine/src/session_drain.rs) and [C bridge](../crates/mtproto-ffi/src/lib.rs).

One worker owns each session. Socket readiness and commands are processed on that worker. Commands from concurrent senders have the order in which the worker receives them. Host-stream signals have their own ready list. Request and session IDs are allocated independently. Callbacks can come from different workers.

| State | Event and guard | Next state / action | Evidence |
| --- | --- | --- | --- |
| Engine created | Workers and pollers start | Running, each session handle encodes its owning worker | `Engine::new`, `create_session`, `worker_for` |
| Live engine | Session create command | Allocate token block and runtime; register both handle and token mappings | `Worker::drain_commands` |
| Active session | Pause / resume | Pause closes links and probes; resume clears backoff and permits reconnection | `set_paused` |
| Active session | Drain command | Draining; release unsent work and keep requests that can still be answered under the old identity | `drain`, `RpcClient::start_handover` |
| Draining | Result, rejection, reconnect, or new submission | Result completes; resend only under the original identity; release work that would require a new identity; release newly submitted work in the same command turn | `drive_drain`, `Session::hand_back`, `Command::Send` |
| Draining | No held requests, deadline, or key change | Emit remaining releases before one `Closed`; stop links. Recent answers may extend the deadline by five seconds, capped at 30 seconds total | `finish_drain`, `end_drain_for_new_key` |
| Closed by drain | Send / drain / destroy | Send releases while the handle still exists; repeated drain is inert; destroy removes the runtime without another `Closed` | `send`, `drain`, `shutdown`, `Command::Destroy` |
| Active or draining | Explicit destroy | Remove runtime and token mapping, close links, emit one `Closed`. Explicit destroy abandons outstanding work | `Command::Destroy` |
| Live engine | Owner destroys C engine | Disable FFI delivery, shut down workers, then free callback context. Host must keep pointer live through concurrent calls and must not destroy it from a callback | `mt_engine_destroy`, `Engine::shutdown`, [FFI ownership](swift-integration.md#15-ffi-ownership-and-lifetime) |

## 2. Logical RPC and API initialization

[RPC client](../crates/mtproto-core/src/rpc/mod.rs), [wrapping](../crates/mtproto-core/src/rpc/wrap.rs), [handover](../crates/mtproto-core/src/rpc/handover.rs).

Each live request has one submission-order entry. It is parked or represented by a protocol query. Timers, token, dependency, verification and host decision may all gate dispatch. Terminal removal clears the request from the indexes; a late decision for a removed request does nothing. A result can acknowledge API initialization only if the request carried initialization for the current generation.

| State | Event and guard | Next state / action | Evidence |
| --- | --- | --- | --- |
| New | Valid ID, body and wrapper bounds | Park, then dispatch when gates allow. Reject invalid inputs and ignore duplicate live IDs | `send`, `dispatch_ready` |
| Parked | Delay elapsed, token ready, dependencies and decisions resolved | Build current wrappers, enter protocol session, snapshot initialization generation if wrapped | `is_ready`, `dispatch_ready` |
| In protocol | Success | Remove request, optionally publish matching init hash, emit completion | `handle_session_event` |
| In protocol | Flood, server error, verification, missing token or key | Park for timer or host action under request/role policy, or fail | `handle_error`, `requeue`, `park_for_temporary_key`, `park_for_verification` |
| Awaiting host | Retry allowed / denied, verification or token supplied | Requeue when allowed; reject or fail when denied; a canceled ID cannot be resumed | `decide_retry`, `resolve_verification`, `set_auth_token_ready`, `cancel` |
| Outstanding | Cancel | Remove from indexes; ask to drop the answer if an in-flight message ID exists | `cancel` |
| Outstanding during handover | No original answer still expected, deadline or key change | Release with delivery uncertainty and remaining policy delay; preserve submission order for a dependency still held | `release_handed_over`, `release_all`, `push_released` |
| Initialization unknown | Successful result of a wrapped request for this generation | Store and publish current hash | `needs_initialization`, `handle_session_event` |
| Known or unknown | Environment hash changes, initialization is invalidated, or key initialization state is replaced | Advance generation. Use new state for subsequent wrapping. Old transmitted success may complete its RPC but cannot publish initialization. A wrapper for the unchanged environment can carry forward only while its query is still queued and untransmitted | `update_environment`, `invalidate_initialization`, `set_stored_init_hash` |

## 3. Protocol query, recovery and packet admission

[Session](../crates/mtproto-core/src/session/mod.rs), [handover](../crates/mtproto-core/src/session/handover.rs), [dedupe](../crates/mtproto-core/src/session/dedupe.rs). Explicit query states are `Pending`, `Sent`, and `Unknown`; absence represents removal. Packet processing modes are `Process`, `Replay`, and `AckOnly`.

| State | Event and guard | Next state / action | Evidence |
| --- | --- | --- | --- |
| Pending | Link, usable salt and session policy allow transmit | Sent; allocate message ID and sequence number. Track request, container and receipt evidence separately | `send`, `poll_transmit`, `next_msg_id`, `next_seq_no` |
| Sent | ACK or authenticated answer | Record acknowledgement; remove on result/error. A quick ACK alone does not complete the RPC | `handle_quick_ack`, `handle_packet`, `QueryState` |
| Sent / unknown | Link loss or response uncertainty | Reuse original message ID inside retransmit window where safe; ask server state when evidence is insufficient | `connection_opened`, `connection_closed`, `http_packet_lost` |
| Unknown | Server confirms receipt or announces answer | Wait or request the answer by message ID. If server says it did not receive the request, normal mode may allocate a fresh ID | `ServiceRequest`, `AwaitedAnswer`, `SessionEvent` |
| Sent / unknown | Session reset, lost key or rejected old copy during handover | Release with conservative delivery uncertainty. Normal mode may retry under policy; drain never sends under a new identity | `reset`, `finish_drain_reset`, `hand_back` |
| Active protocol | Accepted inbound packet | Verify authenticated message and session, apply time/ID/sequence/dedupe checks, then process, replay or ACK only. Container descendants inherit the most restrictive mode | `handle_packet`, `Mode::combine`, `DuplicateCheck` |
| Active protocol | Invalid salt, clock offset, unanswered ping, or transport flood | Apply validated correction, wait for appropriate salt, or close and retry on its own deadline. Monotonic retry times remain separate from server time | `merge_salts`, `sync_wall_clock`, `probe_deadline`, `handle_timeout` |

Message IDs belong to a protocol session. Counts of pending and unknown queries are maintained on transitions and checked against the map in debug builds. Socket acceptance, transport ACK, server execution and final result are separate facts. An HTTP hedge can lose a transport attempt without converting the logical query into a new request.

## 4. Authentication handshake and PFS

[Handshake](../crates/mtproto-core/src/handshake.rs) and [PFS runtime](../crates/mtproto-engine/src/session_pfs.rs). The handshake has explicit `WaitResPq`, `WaitDhParams`, `WaitDhGen` and `Done` states. PFS is implicit in permanent/temporary key ownership, binding, hold timers and pending flags.

| State | Event and guard | Next state / action | Evidence |
| --- | --- | --- | --- |
| Waiting for key | Host supplies valid material or generation is allowed | Install eligible host key or begin handshake with new nonce | `set_auth_key`, `handshake_config`, `Handshake::start` |
| WaitResPq | Correct nonce, accepted fingerprint and factorable PQ | WaitDhParams; send encrypted DH request | `Handshake::step` |
| WaitDhParams | Validate nonce, parameters, prime and server DH value | WaitDhGen; derive candidate auth key and send client DH | `Handshake::step`, `finish_dh` |
| WaitDhGen | Valid OK, retry, failure or timeout | Complete and install key, bounded retry with fresh candidate, or tear down and back off | `finish_dh`, `on_handshake_packet`, `HANDSHAKE_TIMEOUT` |
| PFS awaiting temporary key | Matching non-expired host offer, or fresh temporary handshake | Adopt a bound key or install temporary key with application calls held until binding | `take_offered_key`, `take_handshake_key`, `bind_temporary_key` |
| Binding | Valid success / refused / transient failure | Bound and dispatch, regenerate rejected key, or retry with bounded backoff. Permanent-key checks are a separate mode | `observe_pfs_event`, `regenerate_after_refusal`, `check_permanent_key_with_pfs` |
| Bound | Rotation due, key lost, address class changes or permanent key changes | Quiet rotation or hard-deadline retirement; fail possibly executed requests instead of blindly replaying them under another key; drain ends if old key is required | `rotation_times`, `drive_pfs`, `regenerate_temporary_key`, `fail_unanswered_requests` |
| Destroying permanent key | Destroy command and result | Hold requests, send destroy under permanent key; start over on outcome | `hold_requests_for_destroy`, `start_over_after_destroy` |

A host offer must match the datacenter and permanent-key binding and must not revive a refused key. Successful handshake and successful bind are different milestones. Rotating under load must retain prior execution evidence. Rejected material is excluded from subsequent key selection.

## 5. Route selection, DNS, reconnect and network hints

[Session runtime](../crates/mtproto-engine/src/session_runtime.rs), [Auto and HTTP](../crates/mtproto-engine/src/session_http.rs), [WebSocket](../crates/mtproto-engine/src/session_websocket.rs), [route hints](../crates/mtproto-engine/src/route_hints.rs), [resolver worker](../crates/mtproto-engine/src/worker.rs).

| State | Event and guard | Next state / action | Evidence |
| --- | --- | --- | --- |
| Offline, paused, idle, or waiting for key/address | Inputs become eligible or unavailable-network probe is due | Permit attempt. When blocked, avoid opening a new connection | `wants_connection`, `wants_connection_ignoring_network` |
| Address selecting | Literal, cached DNS, or unresolved name | Connect directly, use live cached answer, or wait on coalesced name/port lookup | `pick_address_at`, `resolve_cached`, `ThreadResolver::resolve` |
| Resolving | First accepted result / empty result / timeout | Cache positive or short negative answer; retry with backoff. If still in flight after the lookup timeout, another lookup may be issued for the same endpoint | `Command::Resolved`, `on_resolved`, `defer_connection` |
| Connecting or unproven | Valid server activity / timeout / rejection | Mark route answered, or close and rotate/back off. A socket alone is insufficient proof | `on_established`, `note_server_heard`, `close_failed_connection` |
| Primary suspected | Racer receives valid probe response | Give primary a chance to recover; promote racer only when criteria hold and drop loser | `start_racer`, `handle_racer_io`, `promote_racer` |
| TCP in Auto | Evidence of blocking or prolonged silence | Probe eligible HTTP/HTTPS/WebSocket routes; adopt a verified route, abandon losing probes | `maybe_probe_http`, `adopt_http_probe`, `adopt_websocket_probe` |
| HTTP or WebSocket in Auto | TCP recheck succeeds | Return to TCP and settle after an answer; schedule another alternate-route probe if TCP fails to settle | `maybe_recheck_tcp`, `switch_back_to_tcp`, `settle_back_on_tcp` |
| Current route or network | Addresses, proxy, transport, network identity or availability changes | Invalidate affected probes or timers. Shared route-hint writes reject a stale network generation | `set_addresses`, `set_proxy`, `set_transport`, `note_network_changed`, `RouteHints::set_network` |
| Named or anonymous memory | Evidence, expiry or imported record | Keep recent HTTP-needed evidence and clear it on relevant TCP recovery; bound named records and reject malformed imports | `note_http_needed_for`, `note_tcp_answered_for`, `load`, `report_change` |

A session's worker serializes route replacement and socket handling. Probes own distinct tokens while active. Shared hints are under a mutex and report persisted changes in order. Worker DNS lookups are coalesced by endpoint, with a first-result policy for overlapping retries; the tests here do not simulate resolver behavior across real network changes.

## 6. Wire phases and host-stream lifecycle

[Connection](../crates/mtproto-engine/src/connection.rs), [host stream](../crates/mtproto-engine/src/host_stream.rs), [transport stream](../crates/mtproto-core/src/transport/stream.rs), [SOCKS5](../crates/mtproto-core/src/transport/socks5.rs), [WebSocket](../crates/mtproto-core/src/transport/websocket.rs), [framing](../crates/mtproto-core/src/transport/codec.rs).

| State | Event and guard | Next state / action | Evidence |
| --- | --- | --- | --- |
| Connecting | Socket/host carrier ready | Negotiate SOCKS5 or HTTP CONNECT when configured, otherwise begin WebSocket upgrade or ready transport | `Connection::handle_writable`, `Phase` |
| SOCKS5 negotiating | Method selected, optional auth succeeds, CONNECT replies | Advance method → auth or connect → done. Partial input waits; invalid response closes attempt | `Socks5Handshake::feed` |
| HTTP CONNECT / WS upgrade | Complete valid response | Tunnel or binary frame stream. Invalid or oversized response fails the attempt | `HttpConnectHandshake::feed`, `WsHandshake::feed` |
| Fake TLS waiting | Valid server hello | Enter record mode and flush queued frames. Invalid hello errors; application data uses MTProto encryption | `TransportStream::receive`, `TlsState` |
| Framing | Header, payload, quick ACK, or transport error | Accumulate bounded frame or emit protocol event. Fragmentation across socket reads preserves parse position | `TransportStream::next_incoming`, `Incoming`, `WsDeframer::feed` |
| Host opening | `stream_opened` first | Connected, wake owning worker | `HostStreams::opened`, `HostPipe::is_connected` |
| Host opening | `stream_closed` first | Terminal failure. A late open callback leaves failure intact and does not wake the worker | `HostStreams::closed`, `opened` |
| Host connected | Receive/write window fills | Apply read backpressure or refuse further writes until the host confirms room. Host calls run outside state lock | `received`, `sent`, `HostPipe::read/write` |
| Host connected | Close, with or without buffered data | Deliver buffered bytes, then EOF or error; refuse writes. Keep first close reason | `HostPipe::read/write`, `closed` |
| Engine owns slot | Engine drops or retires pipe | Remove slot; notify the same host that opened it. Later callbacks cannot revive the slot | `HostStreams::release`, `HostPipe::close/drop` |

A closed-before-open slot cannot enter the connected state. A connected slot may still contain readable bytes after the peer closes. The owner retained by a slot is independent of later host replacement.

## 7. HTTP slot, request and response lifetime

[HTTP link](../crates/mtproto-engine/src/http_link.rs), [HTTP runtime](../crates/mtproto-engine/src/session_http.rs), [HTTP parser](../crates/mtproto-core/src/transport/http.rs). HTTP tracks both carrier readiness and the ordered request metadata for each pipe.

| State | Event and guard | Next state / action | Evidence |
| --- | --- | --- | --- |
| Opening / proxy tunnel | Writable, successful negotiation | Ready, admit a bounded number of pending POSTs | `HttpConn::handle_writable`, `has_slot`, `submit` |
| Busy slot | Output writes and response head/body arrives | Match each response to the oldest request on that socket. A probe or long poll has different metadata from a user query | `flush`, `read`, `complete`, `RequestMeta` |
| HTTP parser head | Valid headers, informational response, or invalid head | Skip informational responses; choose fixed length, chunked, close-delimited, or no body; bound head/body sizes | `HttpResponseReader::read`, `BodyFraming` |
| Chunked body | Size, bytes, delimiter, trailer | Advance chunk substate; emit response only on a complete terminator. EOF completes only a close-delimited body | `ChunkState`, `HttpResponseReader::read/finish` |
| Busy slot | Slow head, timeout, network reset or socket failure | Hedge or retire slot. Report only the requests on that slot as lost; let the session reconcile packet identity | `hedge_http_conn`, `fail_http_conn_token`, `note_http_requests_lost` |
| Busy slot | Response belongs to another RPC generation | Discard stale answer before applying it to current session | `on_http_foreign_answer`, `on_http_response`, `RequestMeta` |
| Idle / draining | Need updates or outstanding answers, or no link work | Keep staggered long polls when useful; close idle slots and end drain at its own deadline | `dispatch_http`, `close_idle_http`, `http_deadline` |

## Confirmed issues by impact

| Issue | Baseline sequence and observed result | Fix and regression evidence |
| --- | --- | --- |
| SM-1 · High · Request handover | In one worker turn, `Create → Drain(empty) → Send → Destroy`. Drain emits `Closed`. Send queues `Released`, then destroy removes the runtime before it is pumped and emits another `Closed`. The observer sees `[Closed, Closed]` and no terminal result for the late request. | Publish the draining send's release before consuming the next command and emit `Closed` only on the first transition. `worker::tests::send_then_destroy_in_one_turn_preserves_release_and_single_close` sees exactly `[Closed, Released(false, 0)]`. |
| SM-2 · Medium · RPC initialization | Send a wrapped request for h1; update environment to h2; receive the h1 result. The baseline stores h2 even though that packet never carried h2. Invalidate an outstanding h1 initialization and its late result restores h1. Both suppress a required wrapper on subsequent requests. | Record the generation at wrapping and accept only matching results as initialization evidence. The original RPC still completes. `old_init_result_does_not_acknowledge_a_new_environment` and `an_old_init_result_cannot_undo_invalidation` check stored hash and subsequent wrapping. The binding regression checks that a wrapper already queued for the same environment still proves initialization after the bind, without another wrapper on the next call. The batched-result regression checks that a query already removed by the protocol layer cannot be mistaken for an unsent query. |
| SM-3 · Medium · Host transport | Host close reports failure during open, then a racing open callback marks the same slot connected. The baseline returns `Ok(true)` instead of connection refusal. The transport can start proxy or protocol negotiation on a dead carrier. | Treat failed-open as terminal and ignore repeated or late open callbacks. `host_stream::tests::a_late_open_cannot_revive_a_failed_stream` checks the preserved failure and absence of a spurious wake. |

The four regressions were run against the published baseline with only test code added, then repeated after the fixes. They cover deterministic event ordering with the existing packet, worker and host-stream fixtures. The native workspace suite, seeded explorer and macOS bridge tests cover broader behavior. This evidence concerns local protocol peers and build/link integration. Live datacenter behavior, production DNS changes and all possible schedules are outside these checks.
