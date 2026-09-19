# License and release checklist

Plumb is an independent, modified fork of PlanetScale Lead, licensed
**AGPL-3.0-or-later**. The [LICENSE](../LICENSE) text is authoritative; this is a
practical engineering checklist, not a legal opinion or certification that every
possible distribution/deployment is compliant.

## Measures in this source tree

- Keep the full original LICENSE unchanged, including the no-warranty terms.
- Preserve PlanetScale copyright/license headers in derived source. Do not replace
  original authorship with Plumb authorship. Unchanged inherited language packages
  keep their existing authors/version metadata.
- Mark modified inherited Rust files with a relevant date and a description of the
  modification. The README and [NOTICE](../NOTICE) prominently state modification
  since 2026-09-20 (Pacific/Auckland), fork provenance, licensing and non-affiliation.
- License new Plumb code under AGPL-3.0-or-later. This increment adds no new external
  dependency to the scalar crate and does not update inherited dependency versions.
- Keep the Plumb extension's own `0.1.0` version, name and contributor metadata
  distinct from upstream. This is attribution clarity, not a substitute for the
  license conditions.
- Do not incorporate private TIN source/binaries or private tests into the fork.
  Unchanged upstream helper scripts are retained for provenance only, not run or
  used to import proprietary material. Public Lead is sufficient for local tests.

## Before conveying source or publishing a release

AGPL sections 4 and 5 require preservation of notices, a copy of the license,
prominent modification/date and license notices, and licensing the covered work
as a whole under the applicable terms. Appropriate legal notices for interactive
interfaces must be considered under section 5(d).

Release gate:

1. Include LICENSE, NOTICE, modified source, build scripts, Cargo manifests/lockfile,
   extension control/SQL generation sources, and any other material necessary to
   constitute the Corresponding Source for the exact version released.
2. Publish an immutable source revision or source archive for the actual binary;
   include local patches and avoid pointing only at a moving default branch.
3. Audit the resolved dependency graph and preserve required third-party notices.
   Do not infer that all bundled dependencies share Plumb's license. No complete
   dependency/license inventory is asserted by this milestone.
4. Verify the archive is complete and rebuildable with the documented toolchain.
   Validate licenses/notices in containers and packages as well as in Git.

## Before distributing binaries, containers or packages

AGPL section 6 specifies Corresponding Source options. For a download-based
release, section 6(d) permits equivalent source access from the same designated
place (or clear directions to an equivalent source server) at no additional charge.
Keep the source available for the required period and satisfy other applicable
conditions, including Installation Information where relevant. A repository URL
that does not contain the deployed modifications is not enough.

The current work is a source investigation, not a completed binary-release process.
Do not treat a successful `cargo pgrx package` as license-compliant distribution
packaging by itself: include the required notices/source-access materials too.

## Before remote/network operation

AGPL section 13 requires a modified version supporting remote interaction to
prominently offer its remote users access to the Corresponding Source of that
version through a network server at no charge. The offer must reach the relevant
users; do not assume a GitHub README they never see satisfies this obligation.

A deployer must provide a prominent source link/offer in the interface or service
through which users interact, identify the exact running revision and include its
modifications and relevant build material. Review how this applies to the actual
SQL/application/service boundary with qualified counsel when needed. Adding a
source-URL function alone, or choosing a different operator name, is not a blanket
solution. Network source-offer integration is a deployment/release gate, not claimed
as automated by this proof of concept.

## Names, provenance and support

Plumb is not affiliated with, endorsed by, or supported by PlanetScale. PlanetScale,
Lead and TIN are names used by PlanetScale. Preserve factual upstream attribution
without presenting the fork as the upstream product or implying trademark rights.
The AGPL is a copyright license, not a general trademark grant. This document
adds no new trademark or other restrictions to the software license.

Report Plumb defects to [strigops-io/plumb](https://github.com/strigops-io/plumb/issues).
Coexistence and an eventual compatibility package do not change these obligations.
