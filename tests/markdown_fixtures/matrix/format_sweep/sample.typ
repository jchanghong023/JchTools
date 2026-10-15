= Rate Limiter Design Document
<rate-limiter-design-document>
This document describes the design of the edge rate limiter. The goal is
to protect upstream services from bursts while keeping #emph[legitimate]
traffic smooth. We favor a token bucket over a fixed window because it
tolerates short bursts without a hard cliff at the window boundary.

== Goals and Non-Goals
<goals-and-non-goals>
- #strong[Goal:] enforce a per-client request budget with
  sub-millisecond overhead.
- #strong[Goal:] degrade gracefully when the shared counter store is
  unreachable.
- #strong[Non-goal:] perfect global accuracy across every edge node.
- #strong[Non-goal:] billing-grade accounting --- that lives in a
  separate ledger.

== Algorithm
<algorithm>
Each client key owns a bucket that refills at a steady rate. A request
costs one token; if the bucket is empty the request is rejected with
`429`. The bucket state is a pair `(tokens, last_refill)` updated lazily
on each hit, so idle clients cost nothing.

== Parameters
<parameters>
#figure(
  align(center)[#table(
    columns: 3,
    align: (auto,auto,auto,),
    table.header([Parameter], [Meaning], [Typical],),
    table.hline(),
    [`rate`], [Tokens added per second], [`50`],
    [`burst`], [Maximum tokens the bucket can hold], [`100`],
    [`key_ttl`], [Idle time before a bucket is dropped], [`600s`],
    [`fail_open`], [Allow traffic if the store is down], [`true`],
  )]
  , kind: table
  )

== Failure Modes
<failure-modes>
When the counter store times out, the limiter consults `fail_open`. With
`fail_open = true` it admits the request and logs a #emph[degraded]
event; this trades strict enforcement for availability. Set it to
`false` only for endpoints where over-admission is worse than a brief
outage.

== Rollout Plan
<rollout-plan>
We ship behind a flag and ramp gradually:

+ Enable in #strong[shadow mode] --- evaluate the decision but never
  reject.
+ Compare shadow rejections against real traffic for one week.
+ Flip to enforcing for internal clients, then external ones.

Shadow mode is the safety net: it proves the parameters before a single
real user ever sees a `429`.
