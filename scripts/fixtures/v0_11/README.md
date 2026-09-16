# Published KIP v1 fixture

This isolated generator uses the published `anda_cognitive_nexus` / `anda_kip`
0.11.0 and `anda_db` 0.11.1 releases selected by Anda Brain v0.11.0's lockfile.
It remains isolated from the main workspace's published KIP v2 dependencies.
Its lockfile was seeded from the v0.11.0 tag, then reduced to the generator's
dependencies.

`seed.kip` contains only synthetic memory. The runtime also creates its actual
bundled v1 ontology and the Brain's `$self` / `$system` bootstrap. The resulting
CBOR file holds the complete object-store bytes, not a hand-built v2 schema or
an export that omits database metadata. No model provider is involved.

From the repository root:

```sh
RUST_MIN_STACK=16777216 cargo run --locked --manifest-path scripts/fixtures/v0_11/Cargo.toml -- anda_brain/tests/fixtures/published_v0_11.cbor
RUST_MIN_STACK=16777216 cargo test -p anda_brain --all-features --test legacy_migration
```

This validates the published storage/runtime path; it does not exercise an old
release binary's HTTP server or authenticate against a production instance.
