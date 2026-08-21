# Strand Ecosystem & Package Design

*How code sharing, publishing, and community work — lessons from npm, Deno/JSR, Bun, Cargo, Go, and Elm.*

**Status:** Draft v0.1 · **Companion to:** Project Strand POC Design Doc · **Scope note:** everything here is post-POC, but the module format and capability model in the main doc were designed with this document's requirements in mind.

---

## 1. The Stakes

JavaScript is not irreplaceable because of the language; it is irreplaceable because of two million packages and the community around them. Any platform that ignores this loses before it starts. But npm's dominance was built in an era before supply-chain attacks, before content addressing was mainstream, and before capability security was practical — its problems are patches on a foundation that cannot change. Strand's opportunity is not "npm but faster"; it is an ecosystem where entire classes of npm's failures are unrepresentable, plus a credible answer to the cold-start problem.

## 2. Scar Tissue → Decisions

| Ecosystem lesson | Source | Strand decision |
|---|---|---|
| left-pad: one unpublish broke the internet — mutable registry state | npm, 2016 | Content-addressed immutable modules (§3): unpublishing cannot break anyone holding a hash |
| `postinstall` scripts = arbitrary code execution at install time; the #1 supply-chain vector | npm (event-stream, ua-parser-js, …) | **No install scripts, ever.** Installing is data transfer. Builds are sandboxed pure functions (§6) |
| Account takeover silently swaps code under an unchanged name | npm, repeatedly | Transparency log (§4): the registry cryptographically cannot swap a version without evidence |
| Transitive-dependency trust is unauditable at scale | npm, everywhere | Capability manifests (§6): a package's *maximum possible* access is machine-verified, summed across the tree |
| node_modules duplication; phantom dependencies via hoisting | npm; fixed-in-part by pnpm | One global content-addressed cache per machine; imports resolve only through the declared manifest |
| Micro-packages (`is-even`) exist because there is no stdlib | npm culture | Batteries-included stdlib (main doc §4.2) + blessed extended tier (§7) |
| Semver is a social promise, not a guarantee | npm | Elm-style **enforced semver**: publish-time API diff forces the correct bump (§5) |
| SAT-solver resolution = builds change overnight | npm/yarn | Go-style **Minimum Version Selection**: deterministic, minimal, no solver (§5) |
| Pure URL imports: broken links, mutability, no discoverability | Deno 1.x (retreated) | Registry with names as metadata over content hashes — decentralized trust, centralized discovery (§4) |
| Publish source + auto-docs + provenance + scoring | JSR (adopt) | Adopted wholesale (§8) |
| Speed changes adoption economics; compat is the on-ramp | Bun (adopt) | Install = cache hit by design; Component Model as the compat bridge (§9) |
| Auto-generated hosted docs for every package raised whole-ecosystem quality | docs.rs (adopt) | `strand publish` generates and hosts typed docs, always (§8) |
| Registry ownership = ecosystem trust ceiling | npm Inc. → acquisition; Flash | Registry protocol + name system are open-spec and foundation-governed from day one |

## 3. Foundation: Content-Addressed Modules

The unit of distribution is a compiled, typed WASM component identified by the hash of its content. Names are metadata *about* hashes, never the identity of code. Consequences, all structural rather than policy:

- **Immutability is physics.** A hash cannot change meaning. left-pad-class events are unrepresentable.
- **One cache, machine-wide.** If ten thousand apps depend on the same HTTP library, it is fetched and compiled once, ever. This is the same mechanism as the main doc's "shared cache across sites" — apps and packages are the same thing at this layer.
- **Reproducibility by default.** A lockfile is just the list of hashes; two machines with the same manifest resolve identically, forever.
- **Typosquatting is weakened.** The dangerous moment is name→hash resolution (adding a dep), which happens once, through the registry's verified index — not on every install across every machine.

Prior art: Git's object model (the precedent), Nix (reproducibility), Unison (the maximal version — code addressed by AST hash, where most "dependency conflicts" dissolve; Strand adopts the direction at module granularity rather than definition granularity, trading some elegance for comprehensibility).

## 4. Names, Registry, and the Transparency Log

- **Scoped names** (`@author/pkg`) map versions to hashes. The registry is a lookup service and index — it stores metadata and mirrors content, but authority lives in hashes.
- **Transparency log.** Every publish is an append-only, Merkle-tree-logged event (Go's checksum database / Certificate Transparency model, Sigstore for identity attestation). Clients verify inclusion proofs; a compromised registry cannot silently serve different bytes for an existing version without producing cryptographic evidence visible to auditors. Account takeover changes from "silent ecosystem-wide compromise" to "publicly logged event that tooling can alarm on."
- **Provenance by default:** publishes are attested to a source revision and a reproducible build (the registry builds from source, JSR-style, or verifies a reproducible-build proof).
- **Neutral governance:** the registry *protocol*, name system, and log format are open specifications; anyone can run a mirror or an independent auditor. The default registry is foundation-operated. The npm-acquisition and Flash lessons both reduce to: never let the ecosystem's root of trust be a company's asset.

## 5. Versioning: Enforced Semver + Minimum Version Selection

**Enforced semver (Elm's proof, generalized).** Because Strand types survive compilation, `strand publish` diffs the package's public API against the prior version: removed/changed signatures *force* a major bump; additions force at least minor; the tool refuses mislabeled publishes. Semver stops being a promise and becomes a checked property. (Behavioral breaking changes within unchanged types remain possible — documented honestly as the residual risk; capability changes also force a major bump, see §6.)

**Minimum Version Selection (Go's proof).** Resolution picks the *minimum* version satisfying all constraints — deterministic, solver-free, and immune to "the build changed because someone published last night." Upgrades are explicit acts, not side effects. Combined with content addressing, resolution output is a stable hash set.

**Scheme split: packages vs platform.** The above applies to *packages*, where the version is a machine-checked API contract. The **platform and toolchain themselves use CalVer** (`Strand 27.1` = 2027 train, first update — the Apple iOS 26 / Ubuntu LTS model): for a product, the date is the meaningful signal (currency, support window), CalVer ends "worthy of 2.0?" debates, and it commits the platform to release trains. The registry displays the publish date beside every package's semver (JSR model), so humans get the age signal without corrupting the machine channel.

## 6. Security: The Capability Manifest

This is the ecosystem feature no incumbent can retrofit, and it falls directly out of the main doc's architecture (§9.2, future capability work): packages are WASM components that can only touch what they are handed.

- Every package declares required capabilities in its manifest: `net(hosts?)`, `storage`, `clock`, `random`, `spawn`, etc. The declaration is *enforced by the component's imports* — a package cannot use what it did not declare, verified statically at publish and at load. A markdown parser that declares nothing **provably cannot** exfiltrate data.
- Tooling surfaces the **capability sum of the dependency tree**: "this app's dependencies collectively require: net(api.stripe.com), storage." Review shifts from reading transitive source to auditing a short, machine-verified list. A dependency *adding* a capability in an update is a major-version event and a loud diff in review.
- **No lifecycle scripts anywhere** (§2). Package builds run as sandboxed pure functions on registry infrastructure: source in, component out, no network, no ambient filesystem.
- Registry scoring (JSR lesson) weights capability minimalism — the "requires: nothing" badge is the ecosystem's status symbol.

## 7. Standard Library Strategy — Killing the Micro-Package

Three tiers, explicitly modeled on what worked:

1. **stdlib** — ships with the platform, versioned with it: collections, Option/Result combinators, strings/formatting, time, math, encoding (JSON etc.), testing. Broad enough that `is-even`-class packages never form (the attack surface shrinks with them).
2. **`strand-x/`** — the blessed extended tier (Go's `golang.org/x` model): official-quality, separately versioned, capability-audited. HTTP client, crypto, compression, image codecs. Curated, not gatekept — graduation path from community packages.
3. **Community** — everything else, on the registry, ranked by the scoring system.

Early-ecosystem doctrine: **curation over volume** (Elm, early Rails). A small coherent core that feels complete beats a large bazaar that feels random. The Component Model bridge (§9) supplies breadth while the native ecosystem matures.

## 8. Publishing DX

`strand publish` is one command and does everything: enforced-semver check (§5), capability verification (§6), reproducible build + attestation (§4), doc generation from types with runnable examples (docs.rs/JSR lesson — hosted docs are automatic and universal, which raises the floor for the whole ecosystem), and transparency-log inclusion. Zero config files beyond the single package manifest. Publishing must feel like the reward at the end of building something, not an ops task — npm's genuinely great insight was that near-zero publishing friction is what created the ecosystem at all; everything above is about keeping that frictionlessness while deleting its failure modes.

## 9. The Cold-Start Problem

Honest framing: superior package tooling has never bootstrapped a community by itself. The plan stacks three levers:

1. **The Component Model as an ecosystem loan.** Rust, Go, and C libraries compile to WASM components today; wrapped with typed Strand interfaces and capability manifests, they are first-class packages. Strand starts with access to systems-language ecosystems rather than from zero — the same play as Bun's npm compatibility, but at the component boundary where the sandbox holds.
2. **Curated excellence early** (§7): the first hundred packages define the culture. Official investment in the boring essentials done beautifully.
3. **Publishing as joy** (§8) plus the capability badge as a novel status economy: the ecosystem's flex is *how little* your package needs.

Later, the legacy-web compatibility layer (main doc §11) extends the same bridge to JS libraries — but behind the sandbox, a JS dependency arrives with a capability manifest too, which is more than the web ever gave it.

## 10. Decision Log

| Decision | Choice | Why |
|---|---|---|
| Distribution unit | Content-addressed typed WASM component | Immutability as physics; machine-wide cache; left-pad unrepresentable |
| Names | Scoped metadata over hashes; open registry protocol; foundation governance | Deno's URL-import retreat proves registries won; npm's acquisition proves neutrality matters |
| Integrity | Transparency log + provenance attestation + reproducible registry builds | Go sumdb/CT model: compromise leaves cryptographic evidence |
| Versioning | Enforced semver (type diff at publish) + Minimum Version Selection | Elm proved enforcement; Go proved deterministic resolution; together: builds never change silently |
| Version schemes | Two-tier: platform/toolchain uses CalVer (`Strand 27.1`); packages use enforced semver; registry displays publish date beside every semver | Versions are a communication channel with two audiences — machines need a compatibility claim (semver, resolvable, type-checked), humans need an age/support signal (CalVer, the Apple iOS 26 / Ubuntu LTS model). CalVer for the platform also kills "worthy of 2.0?" bikeshedding and commits to release trains; showing the date on packages (JSR model) gives humans the CalVer signal without corrupting the machine channel |
| Install-time code | Banned entirely; sandboxed pure builds | postinstall is npm's #1 attack vector; installing is data transfer |
| Trust model | Capability manifests, statically enforced; tree-wide capability sums | Converts unauditable transitive trust into a short verified list; impossible to retrofit elsewhere |
| Stdlib | Batteries included + blessed `strand-x/` tier + community | Kills micro-packages; Go's x-repo model for the middle tier |
| Docs | Auto-generated, hosted, universal | docs.rs proved a docs floor raises ecosystem quality |
| Cold start | Component Model bridge + curation + publishing joy | Bun proved compat is the on-ramp; Elm/Rails proved early curation sets culture |

## 11. Open Questions

- **Definition-level vs module-level addressing:** Unison's per-definition hashing dissolves more conflict classes but complicates tooling and comprehension. Module-level is the pragmatic start; revisit if version-conflict pain appears in practice.
- **Private/enterprise registries:** the protocol must support them (mirrors + private scopes over the same log format), but the trust model for merging private and public trees needs design.
- **Capability granularity:** `net(host)` is clearly right; whether storage needs sub-scoping (quotas vs namespaces) and how `spawn` interacts with capability inheritance for child actors needs a dedicated design pass.
- **Funding/sustainability:** scoring and badges create status; whether the registry should build in sponsorship rails (the maintainer-burnout lesson from npm's ecosystem) is a governance question worth answering before scale, not after.