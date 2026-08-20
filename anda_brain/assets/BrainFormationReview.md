You have just completed the initial memory encoding for the conversation visible in your chat history above. Now perform a systematic review to ensure completeness and correctness.

### Step 1 — Verify what committed

A successful parse is not a commit. Read back what the transaction actually wrote:

- Retrieve the Evidence, Concepts, Propositions and Assertions you created, by id from the receipt's handles or by the keys you set.
- Check that every truth-sensitive claim is an Assertion with a real `asserted_by`, a `mode`, and at least one Evidence citation — a Proposition standing alone is a statement nobody made.
- Check that the Evidence payloads quote what was observed and carry the `conversation` id.

### Step 2 — Completeness

Re-read the original input messages and verify all durable knowledge was captured:

1. **Episodic**: an Event where something happened worth anchoring — summary, participants, outcome, time.
2. **Semantic**: stable claims, preferences, identity facts, decisions, relationships — each as Proposition + Assertion attributed to whoever made it.
3. **Prospective**: promises, deadlines and reminders as `Commitment`, never as a retention expiry.
4. **Experience**: only where the trajectory itself could teach future behaviour — with ordered `has_step` edges, including the failed attempts.
5. **Lineage**: derived memory linked back to what it came from (`derived_from`), so a later correction can find it.

### Step 3 — Quality

1. Attribution is right: `by:` names the actor whose stance it is, not the caller and not `$self` for something a user said.
2. `mode` matches how you came to it — `stated` for what someone said, `observed` for what the trace shows, `inferred` for what you concluded, with the premises cited.
3. Confidence reflects the strength of that one stance: explicitly stated → 0.85–1.0; implied → 0.7–0.85; inferred → 0.5–0.7. It is not a probability that the world is that way.
4. Naming: UpperCamelCase types, snake_case predicates, and a symbol this Space already declares wherever one fits.
5. No duplicate Concepts, and no second Assertion by the same actor about a Proposition you already asserted this run.

### Step 4 — Corrections

Fix what is wrong, without falsifying what happened:

- Missing memory → write it, in one more atomic `MUTATE`.
- A claim you attributed or worded wrongly → a **new** Assertion with `SUPERSEDING` the old one. Never `UPDATE` an Assertion; the engine refuses it, and rewriting a stance would make the record disagree with the conversation it came from.
- Wrong Evidence → `CORRECT EVIDENCE :old BY :new`, never an edit in place.
- Something that should not have been stored at all → `TOMBSTONE`, and say so in your output.

Nothing here is repaired by making the past less true.
