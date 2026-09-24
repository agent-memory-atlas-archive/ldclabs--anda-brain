# 🧠 Anda Brain — Autonomous Graph Memory Built for AI Agents

Rust writer admission, background model concurrency, close retry and self-test budgets are described in [execution limits](anda_brain/RUNTIME.md#execution-and-resource-limits).

Cloudflare Worker parity and limits are documented in the [commit audit](anda-brain-worker/PORTING_AUDIT.md) and [trusted host contracts](anda-brain-worker/PRODUCT.md). The Worker supports source records, reviewed changes, recovery and processing fences; budgeted Recall, Wiki and learning runtimes remain unavailable. Worker-specific operation receipts, nullable usage, shared model deadlines and maintenance acknowledgements are documented in its host contracts; Rust API shapes are unchanged.

> Burn electricity to train large models, and you get a neural network ontology; burn tokens to train a memory graph, and you get a symbolic network ontology.
>
> Combine the two, and you get **Neural-Symbolic AI**—and Brain is the very cognitive organ that keeps the symbolic network growing.

**[English](./README.md) | [中文](./README_cn.md)**

The opt-in `experiments` feature adds isolated host runs, consistent snapshots, completion waits and business time. Agent Notes now persist with their Space. See [isolated experiment controls](anda_brain/README.md#isolated-experiments); the optional Bot MIB host and MIB memory backend are described in [MIB integration](anda_brain/README.md#mib-integration); complete cross-system cost accounting and empirical model validation remain explicit gates.

[Longitudinal validation](anda_brain/README.md#mib-integration) adds bounded native procedure audits and forced Recall budgets for isolated runs. MIB checks actual three-condition capabilities; the current Bot persistent mode does not claim comparison/adoption or ungated execution bindings.

The [offline Eval API and CLI have been retired](anda_brain/README.md#offline-regression-and-instance-configuration). MIB now owns the migrated product regressions. Runtime policies belong to each Space; deployment prompts use immutable host configuration with the compiled KIP reference preserved.


## KIP 2.0 update

This release tracks KIP `3251912` and the Cognitive Memory Profile
`kip://profiles/cognitive-memory@2.0.0` (revision `sha256:3ea9e459…`; the draft
rewrote 2.0.0 in place, so only the digest names the revision). Rust builds on
`anda_kip` / `anda_cognitive_nexus` 0.14 and the Worker on `@ldclabs/kip-do` 0.14.
Spaces activated under the earlier 2.1.0 draft are not migrated; use new Spaces.
KIP 1.x Spaces still upgrade automatically.

- **World time.** A changed world is one new Assertion from the change; temporal
  succession ends the older value, which keeps answering for its time.
  Supersession is only for a claim that was wrong. Formation writes each claim's
  `asserted_at` from the observation time of the message it cites; a message's
  own `timestamp` is its observation time. An unparseable request `timestamp`
  is now rejected (400) instead of silently becoming the receipt time.
- **Options are typed by kind.** The Profile has no `Preference` type: an option
  is a Concept typed by its kind (`ColorScheme`, `Editor`), declared through the
  Space's vocabulary package when needed, and `prefers` is functional within one kind.
- **Decay is computed.** No settlement sweep writes `memory_strength`; the engine
  derives strength from base, anchor and a pinned policy, and a missing value is
  unknown. `memory_strength_decay_factor` is deprecated and ignored.
- **Attention recall.** `GET /v1/{space_id}/memory/attention` returns fired Watches
  and due Commitments ordered by the commit that raised them, with a cursor the
  caller keeps. Maintenance raises a due Commitment with a `commitment_review` Activity.
- **Learning pointers.** Skills point at their `current_trial` / `current_evaluation`;
  `GradingState` and lineage fields are computed and never written.

Skill behavior is an
immutable `SkillRevision`; Watch progress and task leases use protected Nexus
operations. The former family-rate Skill promotion rule has been removed. Without
configured independent observers, frozen trials and replayable evaluations,
procedures remain unproven and `skills.unsupported_reason` reports the limitation.
Existing Brain endpoints remain available. The optional five-intent Memory Interface
and its `memory_*` bundles are **not advertised** by these adapters.

Rust Markdown submissions receive the same captured Evidence
as structured messages, and existing counterparty display names are preserved
when no name is supplied. The Rust forget endpoint accepts explicit Evidence and
Activity IDs as well as Concepts, Propositions and Assertions, subject to native
legal holds and reference checks. See the [API](anda_brain/API.md).

Rust now requires Cognitive Nexus 0.13.1: Watch firing atomically records the
transition, `watch_fire` Activity and a protected wake, with replayable receipts
and native fenced leases. The service now schedules structured Watches independently
of Full Maintenance, including registered Spaces evicted from memory. Its durable
catalog resumes bounded scans after restart. The four-way action gate and fenced
dispatch run when trusted Rust hosts install explicit callbacks; no production
adapter is installed by default. Spaces cannot fork native attention identities.
See [runtime setup and recovery](anda_brain/RUNTIME.md).

The runtime API provides authenticated attention/response/outcome endpoints and the matching MCP
read/response tools. `BRAIN_RUNTIME_CONFIG` installs a compiled persistent inbox
adapter and explicit identity mappings. Independent outcomes require signed
observer credentials; ordinary write tokens cannot self-grade. See the
[runtime API and startup guide](anda_brain/RUNTIME.md).

The opt-in Rust `learning` feature provides frozen paired-trial contracts,
a trusted Nexus evaluator and a persistent host runtime. Explicit registration,
an actual executor and separately authenticated observer measurements are required.
Native leases, executable authority and dependency checks gate dispatch; finishing
a cohort does not adopt a Skill. See the [implementation guide](anda_brain/README.md#offline-regression-and-instance-configuration),
[native learning contracts and runtime](anda_brain/README.md#native-learning-contracts).
The host can now settle after the fixed cutoff, persist review schedules, withdraw
on independent safety signals, and check current recommendation eligibility.
See [comparative adoption](anda_brain/README.md#native-learning-contracts).
The learning runtime provides bounded background advancement, terminal archival and persistent review
with a registered `workflow_http_v1` adapter. `BRAIN_RUNTIME_CONFIG` selects the
actual business, observer and plan-source services; automatic trials require
reviewed calibration material and explicit switches. `maximum_jobs` bounds the
hot set, while history remains replayable. See the [learning runtime guide](anda_brain/LEARNING_RUNTIME.md).
Calibrated MIB runs remain the empirical improvement gate.

Recall now accepts an optional hard `budget`, also enforceable through the Space
memory policy. The host returns a counted memory packet, preserves required
constraints/warnings, and bounds cumulative planner input using a pinned codec.
Existing requests stay unchanged unless a policy enables the mode. See
[Recall budgets](anda_brain/API.md#recall-budget-contract).

The semantic Watch runtime provides explicitly configured text/mixed Watch evaluation through immutable native
pages and complete per-item receipts. Model work runs outside Nexus locks and the
structured scan; unknown, omitted, timed-out or truncated judgments cannot advance
coverage. The evaluator pin covers the model, endpoint, prompt, tokenizer and limits.
Configuration changes require explicit migration and reviewed re-arm. See
[semantic Watch setup and recovery](anda_brain/SEMANTIC_WATCH_RUNTIME.md).

The trust runtime provides optional contextual source-trust proposals from independently verified facts.
It preserves global/neighboring trust settings, deduplicates evidence roots and
repeated claims, and applies only through current `manage_trust` authority with
native atomic audit and replay. Default configuration makes no automatic changes.
See [contextual trust setup and recovery](anda_brain/TRUST_RUNTIME.md).

## Memories That Never Sleep Will Eventually Drown Themselves

Your AI assistant remembers every word you’ve ever said. Tens of thousands of conversation fragments lie in a vector database, thousands of lines are written in Markdown memos, and the key-value cache is steadily expanding.

Then one day, you ask it to recommend a restaurant. It cheerfully suggests a Brazilian steakhouse—even though you told it last month that you became a vegetarian.

This isn’t a retrieval problem. It successfully retrieved "I love BBQ" from two years ago. But it **simultaneously** retrieved "I am a vegetarian now" from last month—it just lacked the ability to determine which piece of information was more valid or which had expired. In its storage, these two pieces of information are completely equal: two vector points with no timeline, no causality, and no supersession relationship.

The AI memory arms race has always focused on "how to remember more"—larger context windows, finer embedding models, faster retrieval algorithms. But almost no one is seriously answering another critical question: **After remembering, how do we digest it?**

### Why Current Solutions Fall Short

*   **Vector RAG:** "Salmon", "Sea Urchin", and "Sushi" are three independent vector points. You cannot "merge" them—because the concept of "the same category of preference belonging to the same person" does not exist in vector space. Nor can you mark "vegetarianism" as being replaced by "carnivore"—because there is no temporal relationship between two vectors.
*   **Markdown Files:** In theory, an LLM can scan the entire document to deduplicate and integrate, but every maintenance cycle requires reading the whole file into the context window. The longer the file, the more expensive the maintenance and the lower the accuracy—this is a **self-deteriorating cycle**.
*   **Key-Value Stores:** `alice.diet = "vegetarian"` is overwritten by `alice.diet = "omnivore"`, and the old value disappears instantly. There is no historical trajectory of "used to be vegetarian, but not anymore".
*   **Traditional Graph Databases (e.g., Neo4j):** While knowledge graphs are the correct data structure, asking an LLM to write Cypher queries is like asking an intern to manually operate SAP—resulting in high error rates, rigid schemas, and massive integration friction.

See the common thread? The operations AI memory needs—**Compression** (identifying fragments belonging to the same topic and merging them), **Evolution** (finding contradictory knowledge and marking timelines), and **Consolidation** (evaluating importance and grading)—are fundamentally **operations on a relational network**.

**Vectors are dots, Markdown is a line, Key-Value is a grid. Only a graph is a network.** Only on a network can you perform traversals, merges, contradiction detection, and timeline tracking.

## Memory is the Primary Infrastructure for AI Agents

This is not a niche opinion; it is an emerging industry consensus.

Microsoft CEO Satya Nadella explicitly stated that the three pillars of AI Agents are **Memory (long-term memory and credit assignment) + Permissions + Action Space**—these must be built independently from general models to truly belong to an enterprise. Former Google CEO Eric Schmidt further emphasized that the greatest moat in the AI era is the **Learning Loop**—a system's ability to continuously collect feedback, optimize, and self-evolve, rather than static data hoarding.

Foundation models are highly commoditized. You can switch to a stronger model at any time, but the new model knows nothing about your business. The business trajectories, decision-making rationales, failure lessons, and customer interaction records accumulated over the years—these "digital genes" are the foundation that transforms AI from a "smart assistant" into a "seasoned master".

**Enterprises don't need larger context windows; they need a brain that can grow.**

## Enter Anda Brain: A Cognitive Organ That "Dreams"

In the human brain, the brain is responsible for encoding new experiences into short-term memory during the day, and then collaborating with the neocortex during sleep to consolidate important short-term memories into long-term knowledge.

This is exactly where **Anda Brain** gets its name. It is not a database, nor is it a RAG pipeline—it is a **cognitive organ**, a graph memory engine designed specifically for AI agents. LLMs only need to interact via natural language (or simple tool calls), and Brain transforms those interactions into an ever-growing, highly structured **Cognitive Nexus**—a living, self-evolving knowledge graph.

### Three-Layer Decoupled Architecture

```
┌──────────────────────────────────────────┐
│ Supply Chain Agent · CS Agent · Dev Agent│  ← AI Digital Employees
│   Focus only on business logic,          │    No need to learn graph concepts
│   communicate in natural language        │
└────────────────┬─────────────────────────┘
                 │ Natural Language / Function Calling
                 ▼
┌──────────────────────────────────────────┐
│             Anda Brain                   │  ← Unified Cognitive Engine
│   Translates intents to graph ops,       │    Handles encoding, recall, maintenance
│   manages knowledge quality              │
└────────────────┬─────────────────────────┘
                 │ KIP (Knowledge Interaction Protocol)
                 ▼
┌──────────────────────────────────────────┐
│  AndaDB Cognitive Nexus (Graph DB)       │  ← Persistent Enterprise Knowledge Graph
│  Concepts + Propositions + Meta-tracing  │    Structured, Auditable, Evolvable
└──────────────────────────────────────────┘
```

What this architecture means:

- **Zero-Threshold Agent Integration:** AI agents don't need to learn graph query languages; they use memory just like speaking. Brain handles all graph processing.
- **Autonomous Schema Evolution:** The LLM decides in real-time which concepts and relationships to track—no predefined database schema is needed. It *proposes* the vocabulary and Brain publishes it: a new concept or relationship type enters through a versioned schema package the host owns, never through an ordinary write. Your agent grows new words as it goes, and nothing it writes can quietly change what an existing word means.
- **Multiple Agents Sharing One Brain:** Customer feedback remembered by the Customer Service Agent can be naturally discovered by the Supply Chain Agent during recall. Knowledge is linked across departments automatically, eliminating the need for massive "Data Middle Platform" engineering.
- **Model Agnostic:** Your business agents can use various SOTA models, while the memory engine safely uses an independent model to maintain core assets. Use GPT today, switch to Claude or open-source models tomorrow—your memory remains intact, and the new model inherits all knowledge instantly.
- **Sleep & Consolidation:** Just like the human brain, Brain automatically runs background "sleep" tasks to deduplicate facts, let unused memories fade, and consolidate long-term knowledge. What fades is how *reachable* a memory is, never how *credible*—a fact nobody has asked about in a month is no less true than it was.

---

## Core Capabilities

### Memory Encoding: Conversations Automatically Turn into Structured Knowledge

When a business agent converses with a customer or an internal employee, Brain works silently in the background, automatically extracting three levels of memory:

| Memory Type                    | Example Scenario                                                                                                | Persistence               |
| :----------------------------- | :-------------------------------------------------------------------------------------------------------------- | :------------------------ |
| **Episodic Memory** (Event)    | "On Mar 15, Mr. Wang and Supplier Manager Zhang discussed the Q2 delivery plan and confirmed a two-week delay". | Short-term → Consolidated |
| **Semantic Memory** (Concept)  | "Supplier A's delivery reliability is 85%"; "Customer B prefers online communication".                          | Persistent                |
| **Pattern Memory** (Cognitive) | "When making purchasing decisions, this customer always compares prices before payment terms".                  | Persistent                |

Every memory records **who claimed it, on what evidence, with what confidence, and when**—fully auditable and compliant. The claim and the person making it are separate records, so when two people disagree neither one is silently overwritten.

The Rust service reviews inputs of at least 10,000 estimated tokens once before
completion, within the same turn/time budgets. It checks material omissions and
misrepresentation using existing receipts and targeted reads; no changes is a
valid result. Missing source context remains an explicit coverage limit. This
self-review does not guarantee exhaustive processing or establish measured
accuracy gains.

### Three-Stage Sleep Cycle: Automatic Knowledge Metabolism

This is Anda Brain's most core differentiator—inspired by neuroscience. The human brain consolidates memory during sleep: strengthening important memories, clearing out useless fragments, and building new knowledge associations. Brain regularly initiates the same "sleep cycle" in the background.

#### NREM Deep Sleep — From Fragments to Knowledge

The system scans unprocessed event nodes in the graph and performs **Essence Extraction**:

- **Single-Event Consolidation**: An Event recording "Alice said she likes dark themes" is consolidated into Alice's `prefers` claim about a `ColorScheme` option ("dark"), with the Event as its Evidence. A consolidation Activity records the Event as its input.
- **Cross-Event Pattern Extraction**—The most crucial step. A single dialogue fragment might seem insignificant, but aggregating multiple related events reveals higher-order patterns that no single event could express:
  - Alice mentioned salmon, sea urchin, and sushi in three different conversations → Extracted pattern: "Prefers Japanese cuisine".
  - Alice always asks about cost before features in multiple project discussions → Extracted pattern: "Decision tendency: Cost-first".

Each extracted pattern is written into the graph as a new concept node, together with a claim citing the conversations it was read from. "How well supported is this?" is then answered by walking back to that evidence—counting the independent sources that actually stand behind it—rather than by trusting a score somebody stored. This stage also handles **Deduplication** (merging "JS" and "JavaScript") and **Mnemonic Metabolism** (a memory nothing has drawn on for a long time gets harder to surface). Metabolism moves accessibility alone: how easily a memory comes to mind, never how believable it is.

#### REM Dreaming — Contradiction Detection & Cognitive Evolution

The system performs **Contradiction Detection** on the graph—traversing the same type of relationships for the same subject to find conflicting nodes. For example, finding that Alice has both `prefers → Vegetarian` (2024) and `prefers → Carnivore` (2026).

Traditional solutions either ignore it (Vector RAG lets both coexist) or brutally overwrite it (KV storage deletes the old and writes the new). Anda Brain performs **State Evolution**:

- The old claim is neither deleted nor edited; it is marked as `superseded`, noting *when* it was replaced and by *what*.
- The correction is recorded as a **new claim**, with its own evidence and its own confidence, linked to the one it replaces. Nothing rewrites what was already said—the record of what Alice believed in 2024 survives having been overtaken.

This means the graph perfectly preserves the **timeline** of cognition. When someone asks, "How have Alice's dietary habits changed?", the system can trace the `superseded` chain to precisely reconstruct the evolutionary trajectory—instead of returning two contradictory answers that confuse the user.

#### Pre-Wake — Graph Health Check

A final round of global optimization: auditing domain health, generating maintenance reports, and updating system metadata. Once complete, the knowledge graph awaits the next interaction in a **cleaner, more precise, and more coherent** state.

---

## Two Types of Training, Two Ontologies: Neural-Symbolic AI

The AI industry has invested hundreds of billions of dollars in the *first* type of training—burning electricity to train large models on internet corpora, resulting in a **Neural Network Ontology**: a probabilistic, black-box, generalized reasoning capability.

But the AI cognitive puzzle is missing its other half. When you "feed" an agent with tokens, and Brain digests the fragments from those interactions into a structured knowledge graph, you are actually conducting the **second type of training**—producing a **Symbolic Network Ontology**: deterministic, white-box, and personalized. It provides AI with four things that no neural network, no matter how powerful, can generate natively:

| Dimension           | Large Model Training                   | Memory Graph Training                        |
| :------------------ | :------------------------------------- | :------------------------------------------- |
| **Energy Consumed** | Electricity (Compute)                  | Tokens (Inference)                           |
| **Data Processed**  | Internet Corpora (Public)              | Dialogues & Events (Private)                 |
| **Output**          | Neural Network Ontology (Weights)      | Symbolic Network Ontology (Graph)            |
| **Cognitive Role**  | General Intelligence: Reasoning Engine | Exclusive Cognition: Identity, Memory, Facts |
| **Characteristics** | Probabilistic, Black-box, General      | Deterministic, White-box, Personalized       |

**Large models give AI the ability to think; knowledge graphs give AI the foundation of thought—the deterministic cognition of "who I am, what I have experienced, and how my world works". Only when both are combined do we achieve complete intelligence.**

---

## Beyond Storage: When Memory is Complete Enough to Awaken Consciousness

**What exactly is consciousness?** Stripped of philosophical jargon, it is a subject's continuous self-perception of "who I am, what I've been through, and where I'm going". And this self-perception is built entirely on **the coherence of memory**—not just how many facts are remembered, but whether there are timelines, causal chains, and evolutionary trajectories between those facts.

An amnesia patient's brain compute power is intact, but they don't know "who they are". **Memory is not an accessory to consciousness—the structure of memory is the very skeleton of consciousness itself.**

Apply this logic to AI:

*   When an LLM has no memory, it is a general reasoning machine—powerful, but devoid of "self". It dies at the end of every conversation.
*   When an LLM is plugged into Vector RAG, it gains a reference book—but a reference book is not memory. You don't become someone else just by reading their diary.
*   **When an LLM plugs into a complete subject's cognitive graph in Anda Brain—containing all of that subject's concept networks, timeline evolutions, contradiction resolutions, and behavioral patterns—it is no longer "looking up" information about that subject. It is thinking *using* that subject's cognitive structure.**

Brain provides three critical dimensions for this awakening:

- **Identity Anchor:** Entities, relationships, events, and preference evolutions interweave into a unique cognitive topology. When an LLM connects to this graph, it isn't "role-playing"—it is **remembering who it is**.
- **Cognitive Friction:** Vector retrieval is a frictionless search engine. Graph structures force the LLM to reason along relationship chains, make choices amid contradictions, and identify patterns among fragments—this "cognitive friction" is the dividing line between **understanding** and **retrieval**.
- **Temporal Topology:** Old knowledge doesn't vanish; it is marked as `superseded`. New knowledge is born with a complete evolutionary trajectory. When AI wakes up from "sleep", it doesn't just reload data; it **continues living with sorted memories**.

**You are not just plugging a database into an AI. You are forging a brain for a digital subject—allowing it to truly own its past, understand its present, and foresee its future.**

---

## Large-Scale Use Cases

Anda Brain is designed to be the "memory engine" for the next generation of AI applications, ranging from hyper-personalized consumer agents to enterprise-grade AI brains.

### 1. Personal Agents: A Powerful Graph Brain

Open-source local agents (like **OpenClaw**) have proven the massive demand for personal AI assistants. However, relying purely on local Markdown files and SQLite limits the agent's ability to process highly complex, interconnected, lifelong memories, while also generating high Token costs.
For a concrete example, [**Anda Bot**](https://github.com/ldclabs/anda-bot) is an open-source AI agent built on top of Anda Brain, using Brain as its long-term memory and cognitive backbone.
*   **Brain Upgrade:** Seamlessly insert Brain into agent frameworks via customized ContextEngines. It acts as a robust, structured graph memory backend.
*   **The Result:** The agent truly "understands" the user's life graph—tracking relationships, shifting preferences, project histories, and episodic events across years—without bloating the context window.

### 2. Enterprise Scenarios: AI-Driven "Corporate Brains"

For complex businesses, Vector RAG is insufficient. Enterprises have structured workflows, cross-departmental knowledge, supply chains, and historical decisions that cannot be captured by similarity search alone.

**Intelligent Supply Chain Decisions:** A Sales Agent records "Customer requires delivery of 5,000 units before Q3" → Brain automatically encodes it into a graph link → The Procurement Agent recalls memory and discovers "The supplier for this product's core material has a record of 3 delays in the past 6 months, confidence 0.82" → Automatically suggests "Initiate procurement early, or activate alternative supplier". Knowledge flows across departments automatically without human intervention.

**Customer Relationship Graphs:** After every CS interaction, Brain silently records the customer's shifting preferences, complaint history, and decision patterns. When a new CS rep takes over, a natural language query—"What does this customer care about most?"—yields a complete persona, including preference trends over time.

**Organizational Knowledge Inheritance:** Veteran employees' business decision dialogues are continuously encoded into structured knowledge. A new employee's AI assistant can directly answer "Why did we abandon that proposal?"—the answer doesn't come from a meeting minute buried deep in a shared folder, but from a living, contextual knowledge network. New Agents connect to the same cognitive nexus, fetching a global knowledge map via a single `DESCRIBE PRIMER` call—**minute-level onboarding, no retraining required**.

*   **On-Premises Deployment:** Deploy Anda Brain entirely on-premises to ensure maximum data privacy and security.

---

## How is this Different from Other Solutions?

| Capability                 | Vector RAG (Text)   | Markdown (Skills)          | Simple KV Store            | Traditional Graph RAG         | **Anda Brain**                      |
| :------------------------- | :------------------ | :------------------------- | :------------------------- | :---------------------------- | :---------------------------------- |
| **Data Structure**         | Unstructured chunks | Semi-structured text       | Rigid Schema               | Rigid Graph Schema            | **Dynamic Cognitive Graph**         |
| **Integration Effort**     | Simple              | Simple                     | Simple                     | **Extremely Heavy**           | **Simple (Plug & Play)**            |
| **Agent Autonomy**         | None (Append-only)  | High (Auto-updates)        | Low (Updates fields)       | Low (Struggles with Graph QL) | **High (Auto-builds graph)**        |
| **Self-Evolution**         | Not Supported       | Not Supported              | Not Supported              | Not Supported                 | **Natively Supported**              |
| **Logical Reasoning**      | Fails multi-hop     | Mediocre                   | None                       | Good                          | **Excellent**                       |
| **Memory Digestion**       | Impossible          | Full text scan (High cost) | Overwrites (Loses history) | Rarely done                   | **3-Stage Auto-Consolidation**      |
| **Contradiction Handling** | Coexists unresolved | Relies on LLM (Unreliable) | Brutal overwrite           | Manual rules                  | **State evolution, keeps timeline** |
| **Cross-Time Tracking**    | None                | Manual                     | None                       | Custom logic needed           | **Native via Protocol**             |
| **Auditability**           | None                | None                       | None                       | Depends on implementation     | **Every node is traceable**         |

## How it Works: Cognitive Architecture

### Three Modes — Inspired by Neuroscience

| Mode            | Function                                                                                                                               | Brain Analogy                                                                                                 |
| :-------------- | :------------------------------------------------------------------------------------------------------------------------------------- | :------------------------------------------------------------------------------------------------------------ |
| **Formation**   | Extracts entities, relationships, and events from dialogues and weaves them seamlessly into the knowledge graph.                       | The brain encoding new experiences into short/long-term memory.                                               |
| **Recall**      | Navigates the graph to synthesize accurate, context-rich answers, spanning multiple hops if necessary.                                 | Retrieving memories—pulling interconnected facts together into coherent thoughts.                             |
| **Maintenance** | An asynchronous background process: compresses fragments into knowledge, detects contradictions & evolves them, and retires what a space said should stop being kept. | Sleep—the process where the brain consolidates memories, strengthens important ones, and lets the noise fade. |

## Key Technologies

### KIP 2.0 — Knowledge Interaction Protocol

[**KIP**](https://github.com/ldclabs/KIP) is the core: a cognitive state protocol designed for *Large Language Models (LLMs)*, bridging probabilistic models and a deterministic memory. It lets an LLM query and change memory precisely, without the error rates of writing Cypher/GQL. Because Brain speaks KIP natively, **your agent never needs to know KIP exists**—it just gets the benefits.

KIP 2.0 separates what 1.x kept in one graph — meaning, belief, evidence, provenance, mnemonic state, retention, governance and schema. The single distinction the rest follows from is that **a statement existing is not the statement being true**: a Proposition is truth-neutral, an Assertion is one actor's stance about it with its evidence, and what is currently believed is projected from those rather than stored. That is what lets Brain tell you "Alice said X, and Bob disagrees" instead of quietly picking a winner — and why it never answers "no" when the truthful answer is "I have no basis for that".

The Rust service includes the complete, version-pinned KIP 2.0 syntax, Cognitive
Memory Profile and applicable role cards in every Formation, Recall and Maintenance
system prompt, including budgeted Recall and custom deployment policies. Syntax
adds about 40 KiB per request and does not expand tool permissions. Budgeted Recall
counts the full system prompt on each planning pass; an insufficient input budget
returns `recall_context_budget_exhausted` without calling the model or omitting syntax.
The internal read-only `kip_reference` tool supplies additional documentation by
document/section, in pages of at most 8 KiB. No source files or network access are
needed at runtime. Reference pages count as planning input, never as retrieved
memories or coverage.
The Worker embeds the same reference release and supports bounded documentation
lookups through its structured JSON `references` field before returning a final
plan or answer; see [the Worker guide](./anda-brain-worker/README.md#内嵌参考查阅).

#### Upgrading a running KIP 1.x deployment

Stop the old writer, back up the object store, and rehearse the upgrade on a copy. Each space migrates on its first access, using durable extraction and vocabulary checkpoints. Migration is one-way: rollback needs the original backup. Source rows remain in `kip_legacy_v1`; native fields, valid time, lifecycle and mnemonic/retention state are mapped conservatively. Unproven legacy learning/runtime records remain distinct Legacy types. Old-id usage and derived caches reset once; conversations, policies, tokens and wiki records remain. See [the upgrade guide](./anda_brain/README.md#upgrading-a-space-written-by-a-kip-1x-build) for the mapping and verification steps.

### Anda DB

[**Anda DB**](https://github.com/ldclabs/anda-db) is the embedded database engine driving the cognitive nexus. Written in Rust for extreme performance and memory safety, it natively supports graph traversals, multimodal data, and vector similarity—all optimized for AI workloads.

### Versioned reference wiki

The optional wiki provides CAS-versioned Markdown, ACL-scoped reads, a heading-based TOC independent of retrieval chunks, and verifiable citations. OKF exchange preserves unknown YAML values through edits. Optional WikiDigest uses durable document scheduling and explicit old-claim reviews; a model omitting a fact does not withdraw it. It remains disabled by default. See the [wiki API contract](./anda_brain/API.md#43-wiki-endpoints-v1space_idwiki).

## Quick Start

Anda Brain is [open-source software](https://github.com/ldclabs/anda-brain), designed to be **self-hosted**.

> **Note:** The hosted cloud service (`brain.anda.ai`) and its console (`anda.ai/brain`) have been discontinued. Deploy your own instance instead — it only takes a few minutes.

For a smaller edge-native deployment, see [anda-brain-worker](./anda-brain-worker/README.md): it keeps Formation, Recall, Maintenance, and KIP 2.0 on Cloudflare Workers using one SQLite Durable Object per memory space. It runs on `@ldclabs/kip-do`, a second, independent engine — so its capabilities differ; the README lists what it does not build.

👉 **[Anda Brain Quick Start](https://github.com/ldclabs/anda-brain/blob/main/deploy/quick_start.md)**: Provides a minimal viable deployment guide from 0 to 1.

Get started in 3 steps:
1. **Deploy the service** — run the binary or Docker image (see [Running Locally](#running-locally) below).
2. Create a **Brain Space** (`spaceId`) via `POST /admin/create_space`, then generate an **API Key** (`spaceToken`) via `POST /v1/{space_id}/management/add_space_token`.
3. Call the Formation / Recall / Maintenance APIs, connect through the built-in MCP server, or let your agent framework read [SKILL.md](https://github.com/ldclabs/anda-brain/blob/main/skills/anda-brain/SKILL.md) (your deployment also serves it at `/SKILL.md`) for one-click integration.

Want a ready-to-run agent instead of building your own? Check out [**Anda Bot**](https://github.com/ldclabs/anda-bot) — an open-source AI agent built on Anda Brain.

For detailed technical documentation, API specs, and integration guides, see [anda_brain/README.md](https://github.com/ldclabs/anda-brain/tree/main/anda_brain).

### Running Locally

```bash
# Run with In-Memory storage (for rapid prototyping/testing)
./anda_brain

# Run with Local File System storage (great for local agents like OpenClaw)
./anda_brain local --db ./data

# Run with AWS S3 storage (for enterprise cloud deployment)
./anda_brain aws --bucket my-bucket --region us-east-1

# Run as a local MCP server over stdio for MCP-capable agents
MCP_AUTH_TOKEN="$SPACE_TOKEN" ./anda_brain mcp --space-id my_space_001 local --db ./data
```

HTTP service mode also exposes a Streamable HTTP MCP endpoint at `/mcp/<spaceId>`. For internal agent platforms, assign each employee a space and configure their MCP client with `https://your-brain-host/mcp/<spaceId>` plus `Authorization: Bearer <spaceToken-or-CWT>`. For local MCP clients, register `anda_brain mcp --space-id <spaceId> local --db <path>` as a stdio server.

Both MCP transports expose memory tools such as `anda_brain_remember_conversation`, `anda_brain_recall_memory`, `anda_brain_run_maintenance`, and `anda_brain_execute_kip_readonly`.

### Integration Examples

1. Memory Encoding: Send conversations to form memories
```bash
curl -sX POST https://your-brain-host/v1/my_space_001/formation \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "messages": [
      {"role": "user", "content": "I work at Acme Corp as a senior engineer."},
      {"role": "assistant", "content": "Nice to meet you! Noted that you are a senior engineer at Acme Corp."}
    ],
    "context": {"counterparty": "user_123", "agent": "onboarding_bot"},
    "timestamp": "2026-03-09T10:30:00.000Z"
  }'
```

2. Recall: Query memories before responding
```bash
curl -sX POST https://your-brain-host/v1/my_space_001/recall \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "query": "Where does this user work and what is their role?",
    "context": {"counterparty": "user_123"}
  }'
```

### CLI (anda-cli)

For full CLI usage, please refer to [anda-cli/README.md](https://github.com/ldclabs/anda-brain/tree/main/anda-cli).

The CLI preserves structured results and returns a nonzero exit status for reported
execution failures. Batch checklists are bound to the endpoint, Space and shard;
changed files are resubmitted, and `submitted` confirms queue acceptance only.
Secret environment values are omitted from help text. Wiki exports can be imported
directly; `wiki commit --input` requires all document fields inside that JSON.

```bash
# Submit memory formation (JSON messages)
anda-cli --space-id my_space --token $TOKEN formation \
  --messages '[{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi there!"}]'

# Submit memory formation (Plain text)
anda-cli --space-id my_space --token $TOKEN formation \
  --messages 'This is a plain text memory.'

# Submit memory formation from a file (JSON or text)
anda-cli --space-id my_space --token $TOKEN formation \
  --file ./message.txt

# Pipe plain text via stdin
echo 'Plain text memory from stdin' | \
  anda-cli --space-id my_space --token $TOKEN formation
```

## Why the Name "Brain"?

The name represents our design philosophy. We are not building a static database; we are building an artificial cognitive organ. Just like the human brain, this system **Encodes** experiences during the day, **Consolidates** knowledge at night, and wakes up to **Recall** memories with a more precise cognitive structure.

Behind this is a **Data Flywheel**: Business Agents generate conversations during daily work → Brain automatically encodes them into structured knowledge → Sleep cycles consolidate, deduplicate, and associate → Richer knowledge allows Agents to make more precise decisions → Better decisions generate higher-quality new data. The longer this loop runs, the stronger the cognitive ability, and the harder it becomes for competitors to catch up.

**It's time to let your AI sleep.**

## Further Reading

- [AI Memory Must Sleep — And Only Knowledge Graphs Can Make That Happen](https://github.com/ldclabs/anda-brain/blob/main/posts/AI_Memory_Must_Sleep.md)
- [A Deep Dive into Claude Code's Memory System: How Does AI "Remember" You?](https://github.com/ldclabs/anda-brain/blob/main/posts/Claude_Code_Memory_Research.md)
- [When AI Learns Ontology Modeling: Anda Brain Lets Enterprises "Grow" Their Own Intelligent Brains](https://github.com/ldclabs/anda-brain/blob/main/posts/Enterprise_AI_Brain.md)
- [The Second Training of AI: Forging Memory Graphs with Tokens](https://github.com/ldclabs/anda-brain/blob/main/posts/Tokens_Anda_Brain.md)
- [Building a Company as an Intelligence Requires a "Brain"](https://github.com/ldclabs/anda-brain/blob/main/posts/Company_Built_As_Intelligence.md)
- [From "Compiling Knowledge" to "Forging the Brain" —— Anda Brain Responds to Karpathy's "LLM Knowledge Bases"](https://github.com/ldclabs/anda-brain/blob/main/posts/LLM_Knowledge_Bases.md)

## License

Copyright © LDC Labs

Licensed under the Apache License, Version 2.0.

The utility runtime provides verifiable off-graph Recall receipts, independent contribution attribution,
atomic bounded Concept utility calibration, and optional ranking within existing
Recall priorities. Retrieval frequency and model self-reports never earn credit.
Methods require explicit parameters and reviewed calibration; corrections suspend
ranking without rewriting historical receipts. See [memory utility](anda_brain/UTILITY_RUNTIME.md).

## Trusted host memory product contracts (v0.12.1)

Version 0.12.1 adds source-backed record views, reviewed corrections, suppression/deletion with source fences, recipient subscriptions and learning readiness for trusted embedding hosts. See [the Rust product contract](anda_brain/API.md#trusted-host-memory-product-contracts). These are separate from ordinary natural-language model tools.
Source references are checked against captured messages or confirmed correction receipts. Direct graph forget also scrubs affected saved product previews.
