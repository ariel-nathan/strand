# Inspiration Canon

*Elegant, performant software worth studying — and what each teaches Strand.*

**Companion to:** Project Strand POC Design Doc · **Organizing thesis:** nearly every entry wins by **deleting a subsystem** — finding the design where a whole category of code doesn't need to exist. LMDB deletes the write-ahead log, WireGuard deletes cipher negotiation, esbuild deletes compiler passes, TigerBeetle deletes malloc, immediate-mode UI deletes retained state. Elegance is not polish; it is absence. Strand's own deletions so far: no hydration, no cascade, no try/catch, no z-index, no async keyword, no cookies. Keep hunting.

---

## Databases & Storage — the discipline champions

**SQLite** — The most deployed software on Earth. Zero config, single file, public domain, and a test suite orders of magnitude larger than the library itself. *Lesson: reliability is the feature; small, self-contained, boring interfaces win for decades.*

**LMDB** — A full ACID key-value store as a memory-mapped copy-on-write B-tree in a few thousand lines. No WAL, no cache layer, no background threads — the right core structure deletes entire subsystems. *Lesson: peak elegance is what you didn't build.*

**TigerBeetle** — Static allocation of all memory at startup (no malloc after boot — the arena philosophy at database scale), single-threaded core, deterministic simulation testing running years of simulated failures per day. *Steal for Strand: deterministic message scheduling makes the whole actor VM simulation-testable.*

**Redis (early era)** — Single-threaded event loop + well-chosen data structures beats complicated concurrency for a huge problem class. Among the most readable C ever shipped. *Lesson: choose the data structure first.*

## Languages, VMs, Compilers

**LuaJIT** — A tracing JIT, largely from one person, that outperformed teams of hundreds for years. Lua itself: a complete embeddable language built on one data structure (the table). *Lesson: constraint as design.*

**BEAM/OTP** — Strand's §5 as running software: per-process heaps, reduction-counting preemption, hot code loading, supervision. *Study the implementation choices, not just the ideas — you will face each one directly.*

**Chez Scheme** — The nanopass compiler: dozens of tiny passes, each one trivially verifiable transformation. *Steal for Strand: this compiler shape stays debuggable as it grows.*

**Turbo Pascal** — Compiled, edited, and ran in 64K, with feedback loops faster than modern toolchains feel today. *Ancestor of the one-binary, instant-feedback DX doctrine.*

**TeX** — Version number converges to π; essentially bug-free for decades; still typesets most of mathematics. *Lesson: software can be finished.*

**esbuild** — 100x faster than incumbent bundlers via: fast language, parallelism, parse once, refuse features. *The modern witness for every performance-through-architecture claim in the design doc.*

## Systems & Networking

**WireGuard** — ~4,000 lines replacing stacks of hundreds of thousands. Fewer knobs *as* the security model: no cipher negotiation means no downgrade attacks. *The best modern argument for "one obvious way."*

**seL4** — Formally verified, capability-based microkernel that is also the fastest in its class. *Existence proof that capability security costs nothing at runtime when designed in, not bolted on (→ Strand §9.2, future capability work).*

**nginx** — Event-driven master/worker architecture that beat thread-per-connection Apache. *Ancestor of every claim Strand's scheduler makes.*

**ripgrep** — Beats grep at grep's game: finite automata done properly + respect for the memory hierarchy. *Also the gold-standard Rust codebase to read before writing Strand's.*

**qmail / daemontools (djb)** — Security through partitioning: mutually untrusting components, minimal privilege, narrow interfaces. *Actor isolation, shipped in 1998.*

## Graphics, Games, UI — the renderer's lineage

**id Tech (DOOM/Quake)** — Understand the hardware, then design the data flow. The real lesson isn't the fast inverse square root; it's BSP trees: precompute structure so the hot loop does almost nothing. *→ render command arrays: do the thinking at layout time, not paint time.*

**RollerCoaster Tycoon** — A complete, beloved simulation game by one person in assembly. *Not a practice to copy; a ceiling to remember.*

**Dear ImGui** — The immediate-mode UI clay descends from. *Study how debug tooling, input, and layout stay simple when the API refuses retained state.*

**Zed / GPUI** — GPU-rendered, latency-obsessed desktop UI by a small team in Rust. *Strand's M3, shipped as a product — read it when the renderer gets hard.*

## Cross-Cutting

**Git's object model** — An immutable content-addressed DAG with four object types explains the entire system; the famously bad CLI on top is its own lesson. *Literal precedent for Strand's content-addressed module store — and a reminder that an elegant core does not excuse an inelegant interface.*

**Cap'n Proto** — Zero-copy serialization: the wire format is the memory format, so parsing costs nothing. *Directly applicable to render command arrays and channel messages — with the right layout, crossing an actor boundary or a network never deserializes.*

---

## Reading order for Strand specifically

1. **Lunatic source** — §5 of the design doc, already built; learn from both the code and why it stalled.
2. **ripgrep + esbuild** — how to structure the Rust codebase; how to think about performance as architecture.
3. **TigerBeetle's simulation testing** — before writing the scheduler, so determinism is designed in.
4. **clay.h + Dear ImGui** — before M3.
5. **BEAM internals** (e.g. *The BEAM Book*) — before Tier-2 hot reload and supervision edge cases.
6. **Chez nanopass papers** — before the compiler grows past the POC subset.
