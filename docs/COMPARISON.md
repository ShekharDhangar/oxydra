## Comparison: Oxydra vs. ZeroClaw vs. IronClaw vs. MicroClaw

All four are Rust-based AI agent runtimes that emerged from the "\*Claw" ecosystem, although Oxydra strives to be unto its own and not a `*claw`. Each takes a distinct philosophy.

### At a Glance

| | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Author** | Independent | zeroclaw-labs | NEAR AI | microclaw org |
| **Language** | Rust | Rust | Rust | Rust |
| **Version** | 0.1.2 | ~0.1.x | 0.12.0 | ~0.1.x |
| **Maturity** | Pre-alpha | Early | Most mature | Early |
| **Primary focus** | Secure isolated agent orchestrator that evolves and learns | Ultra-lightweight personal bot | Secure always-on agent | Multi-channel chat bot |

---

### Security & Sandboxing

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Isolation tiers** | **3 tiers**: Firecracker micro_vm (strongest) → Docker container → host process | 1: Docker (optional), process-level security | 2: Per-tool WASM + Docker job sandbox | 1: Docker (on/off switch) |
| **Firecracker VM isolation** | **Yes** (Linux; Docker VM on macOS) | No | No | No |
| **WASM sandbox** | **Yes — on ALL tiers** (wasmtime v41 WASI preopens, hardware-enforced capabilities) | Planned, not shipped | Yes — per tool | No |
| **WASM capability profiles** | **5 distinct profiles**: FileReadOnly, FileReadWrite, Web, VaultReadStep, VaultWriteStep | N/A | Yes, capability declarations | N/A |
| **Vault with 2-step semantics** | **Yes** — read and write are separate atomic ops linked by operation_id; vault never simultaneously readable+writable | No | Credential injection at boundary, not 2-step | No |
| **Output scrubbing** | **Yes** — path redaction + keyword scrubbing + **entropy-based detection** (Shannon ≥3.8 bits/char) | No | Leak scan pre/post execution | No |
| **Depth of defense model** | **5 layers**: runner isolation → WASM capability → security policy → output scrubbing → runtime guards | ~3 layers: allowlists, env hygiene, Docker | ~4 layers: WASM, allowlist, leak scan, credential injection | ~2 layers: Docker mode + security profiles |
| **SSRF protection** | **Yes** — IP blocklist + resolve-before-request | Partial (endpoint allowlisting) | Yes (endpoint allowlisting) | No |
| **Per-user workspace isolation** | **Yes** — dedicated guest VM pairs, 4 separate mount points per user | No | No | No |
| **TEE deployment** | Not yet | No | **Yes** (NEAR AI Cloud, verifiable attestation) | No |
| **External dependency for security** | None (embedded) | None | PostgreSQL required | Docker required |

---

### Architecture & Engineering Quality

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Crate structure** | **12 crates, strict 3-layer hierarchy** enforced by compiler | Single-binary monolith | Rust workspace | Single-binary |
| **Dependency enforcement** | **`deny.toml`** license compliance (OSI allowlist), no duplicate crates, supply chain controls | Not documented | Not documented | Not documented |
| **Code quality policy** | **Zero clippy warnings** (denied in CI), 100% test coverage for critical paths | Not documented | Not documented | Not documented |
| **Config system** | **6-layer** precedence: built-ins → system → user → workspace → env vars → CLI flags | YAML flat config | TOML + NEAR AI account required | YAML flat config |
| **Type safety** | Typed identifiers (`ModelId`, `ProviderId`), build-time model catalog validation | Not documented | Not documented | Not documented |
| **Tool dispatch strategy** | **Parallel ReadOnly batch** + sequential SideEffecting; order preserved | Sequential | Parallel (priority scheduler) | Sequential |
| **Documentation** | **15-chapter architectural guidebook** (~120KB) + README + inline docs | README + wiki | README + CLAUDE.md | README + docs site |

---

### Tools & Capabilities

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Built-in tools** | **23 tools**: file (6), web (2), vault, shell, memory (4), scratchpad (3), scheduler (4), delegation, media | Shell, file, web, memory | ~890 WASM skill registry | Bash, file (3), web, memory |
| **Tool macro** | **`#[tool]` proc macro** generates `FunctionDecl` from signatures | Not documented | WASM capability declarations | Not documented |
| **Scheduler** | **Yes** — cron/interval/once, queryable run history, pause/resume | Not documented | Yes (heartbeat/cron routines) | Yes (cron + one-shot) |
| **Multi-agent delegation** | **Yes** — typed specialist agents with per-agent tools, providers, turns | No | Yes (dynamic tool generation) | Via MCP federation |
| **Skill ecosystem** | Self learning + External planned | No external registry | **~890 curated WASM skills** | MCP federation + skills directory |
| **MCP support** | Planned | No | Yes (unsandboxed) | Yes |

---

### LLM & Channel Support

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **LLM providers** | OpenAI (Legacy and Responses), Anthropic, Gemini + OpenAI-compatible proxies | **22+ providers** | 6 backends | OpenAI, Anthropic, Google, Ollama, others |
| **Channel count** | 2 live (Telegram and TUI); framework to easily add more | **15+ channels** (WhatsApp, Signal, iMessage, Matrix, Nostr, QQ…) | 3 (Telegram, Slack, HTTP webhook) | 6 (Telegram, Discord, Slack, Feishu, IRC, Web) |

---

### Memory & Storage

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Memory retrieval** | **Hybrid: vector + FTS5 BM25** (configurable weights) | Hybrid: 70% cosine + 30% BM25 | Hybrid: full-text + vector (RRF) | SQLite + optional semantic embeddings |
| **Embedding backend** | **model2vec-rs (Potion) or blake3 deterministic** — both embedded, zero external model API required | OpenAI `text-embedding-3-small` (requires API) | pgvector | OpenAI or Ollama (external) |
| **Storage engine** | **Embedded libSQL** (Turso optional for remote) | Embedded SQLite | **PostgreSQL + pgvector** (external, mandatory) | Embedded SQLite |
| **External DB required** | **No** | No | **Yes** | No |
| **Session management** | Full turn-level persistence, stale-session archival at 48h | Session resume | PostgreSQL-backed | SQLite + session resume |

---

### Deployment & Footprint

| Dimension | **Oxydra** | **ZeroClaw** | **IronClaw** | **MicroClaw** |
|---|---|---|---|---|
| **Deployment artifacts** | Runner + guest VM binaries + oxydra-shelld + TUI + Docker images | **Single ~16MB static binary** | Binary + PostgreSQL + pgvector | Single binary + YAML |
| **RAM at runtime** | Higher (VM pair per user) | **~5MB** | Higher (Postgres stack) | Moderate |
| **Platforms** | Linux (amd64, arm64), macOS (arm64) | Linux, macOS, ARM, x86, RISC-V | Linux primarily | Linux, macOS |
| **Setup complexity** | Medium (runner + guest images) | Low | **High** (NEAR AI account + PostgreSQL + pgvector) | Medium |

---

### Summary: Where Each Wins

| Project | Strongest at |
|---|---|
| **Oxydra** | Defense depth (5 layers, 3 isolation tiers, WASM on all tiers, entropy scrubbing, vault semantics), self learning, architectural rigor (compiler-enforced boundaries, deny.toml, zero clippy), no external DB required, configurable turn budget, context compaction |
| **ZeroClaw** | Ultra-low footprint (~5MB RAM), broadest channel coverage (15+), most LLM providers (22+), fastest startup |
| **IronClaw** | Largest skill ecosystem (~890 curated), TEE deployment, most mature (v0.12.0), NEAR AI cloud integration |
| **MicroClaw** | Simplest multi-channel chat automation, wide agentic iteration budget (100 turns), context compaction |

Oxydra's distinguishing technical advantages are in security architecture and engineering rigour while still maintaining as much, if not more, flexibility as others and keeping self-learning evolution as a key goal: no other project in this group combines Firecracker-level VM isolation + WASM capability profiles on every tier + a vault with 2-step atomic semantics + entropy-based output scrubbing + a compiler-enforced 5-layer defense model — all without requiring an external database or cloud account.
