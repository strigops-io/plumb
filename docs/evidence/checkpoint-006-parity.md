# Two-instance parity report

**Status: passed**

Action: `compare` · Run: `smoke007`

- plumb: 1008 fixture rows; SHA256 `da01637e2975e9d8f2a9e898d4125769e7dc94a94427611b3f73cdd5ce9a9290`.
- tin: 1008 fixture rows; SHA256 `da01637e2975e9d8f2a9e898d4125769e7dc94a94427611b3f73cdd5ce9a9290`.

| Case | Status | Missing / extra IDs |
|---|---|---|
| common | passed | 0 / 0 |
| medium | passed | 0 / 0 |
| rare | passed | 0 / 0 |
| and | passed | 0 / 0 |
| or | passed | 0 / 0 |
| absent | passed | 0 / 0 |
| phrase | passed | 0 / 0 |
| not | passed | 0 / 0 |
| wildcard | passed | 0 / 0 |
| unicode | passed | 0 / 0 |

## Natural-plan timings

All values are milliseconds; these are per-case sample medians, not throughput claims.

| Case | Role | Server planning | Server execution | Client round trip |
|---|---|---:|---:|---:|
| common | plumb | 1.643 | 5.957 | 15.381 |
| common | tin | 1.562 | 95.368 | 102.759 |
| medium | plumb | 1.642 | 0.918 | 10.768 |
| medium | tin | 1.497 | 94.152 | 101.643 |
| rare | plumb | 1.677 | 0.319 | 10.157 |
| rare | tin | 1.505 | 88.732 | 95.839 |
| and | plumb | 1.700 | 1.297 | 10.941 |
| and | tin | 1.521 | 163.840 | 171.586 |
| or | plumb | 1.679 | 1.280 | 11.196 |
| or | tin | 1.657 | 160.711 | 169.046 |
| absent | plumb | 1.666 | 0.257 | 10.006 |
| absent | tin | 1.494 | 116.804 | 124.952 |
| phrase | plumb | 1.668 | 6.888 | 16.910 |
| phrase | tin | 1.571 | 113.100 | 120.722 |
| not | plumb | 1.624 | 9.909 | 19.932 |
| not | tin | 1.701 | 162.439 | 171.466 |
| wildcard | plumb | 1.659 | 100.721 | 111.245 |
| wildcard | tin | 1.797 | 1813.915 | 1823.268 |
| unicode | plumb | 1.799 | 0.808 | 11.190 |
| unicode | tin | 1.720 | 92.917 | 102.740 |

JSON includes ownership/fingerprint evidence, result differences and natural EXPLAIN samples with separate server and client timings.

## Scope and caveats

- Hosted PlanetScale TIN parity is unverified unless this report was produced against an authorized hosted TIN service. An extension name is not provenance.
- Public Lead/local TIN runs validate the harness, not real hosted TIN parity; the TIN label is an unverified operator assertion.
- Endpoint identity is an accident guard, not proof against aliases or proxies.
- Independent servers share no snapshot. Before/after fingerprints detect drift but cannot rule out intervening ABA mutations.
- Different hardware, PostgreSQL majors, services, caches and network prevent generic performance superiority claims.
- This bounded deterministic corpus is not full application or full-scale workload coverage. ANSI highlighting is not tested.
