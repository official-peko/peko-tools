Peko V2 Tooling Roadmap

Legend: **[me]** I implement · **[you]** your checkpoint · **[platform]** web-side, listed for sequence only.

## Phase 0 — Dependency resolution + manifest editing  **[me] ← next**
- [X] **Scope import resolution to `peko.lock`** — discovery/simulator must bind the project's locked versions, not "latest in the global cache"
- [X] **Path dependencies discoverable** — make `{ path = ... }` deps importable by the compiler (resolved + locked today, but not surfaced)
- [X] **Enforce `min_compiler`** — reject/warn when a selected version needs a newer compiler
- [X] **Resolver robustness** — real cross-graph conflict handling (beyond first-wins)
- [X] **`peko add` / `peko remove` edit `peko.toml`** — format-preserving `[dependencies]` edits (`toml_edit`) + re-resolve

### ✅ Checkpoint A **[you]**
Scaffold the current core packages to the new format → test local package resolution end to end.

## Phase 1 — Config correctness  **[me], supporting Checkpoint A**
- [X] Confirm package + project config (`peko.toml`) is fully parsed and operated on correctly across build/run/discovery
- [X] **Remove the mistaken `[features]` surface** — parser, model, index entry, resolver
- [X] Fix anything local testing surfaces

## Phase 2 — Toolchains (`toolchain.toml`)  **[me]**
- [X] Drive compile + link from `toolchain.toml` (parser exists; build still uses the hardcoded `toolchain_sysroot` layout)
- [X] Per-target toolchain install / selection (Apple targets have special setup)

## Phase 3 — Native build  **[me]**
- [ ] Consume the `[native]` table (`sources` / `include` / `flags` / `link` / `[[vendor]]`, incl. `for_os`) in the C compile + final link
- [ ] Build-on-install for a **dependency's** native C sources, per the project's targets

## Phase 4 — Language-side V2 (Parts IV–VI)  **[me]**
- [ ] `Pointer<T>` → `pointer<T>` rename
- [ ] `f16` / `f32` / `f64` floats (drop `double`)
- [ ] Cast surface: `as` (static-safe) + `danger_cast<T>` (forced)
- [ ] `switch` exhaustiveness
- [ ] `serialize` / `deserialize`
- [ ] Remaining Parts IV–VI items (scope from spec when we get here)

### ✅ Checkpoint B **[you]**
Everything builds and runs from your system.

## Phase 5 — Web platform + toolchain downloads  **[platform]**
- [ ] Registry server + static index hosting, R2 blob store
- [ ] Toolchain distribution / download

## Phase 6 — App linking + publishing  **[platform] + [me] CLI side**
- [ ] `peko login` / auth
- [ ] `peko link` → write platform-assigned `app_id` (`Manifest::write_app_id` already exists)
- [ ] `peko publish` upload + index-line append + search mirror (packing is done; emit the index line locally as the handoff)

## Phase 7 — Hosting system  **[platform]**
- [ ] `static` / `server` framework hosting, SSR runtime, domains
- [ ] CLI orchestration for `static`/`server` framework build paths (the tooling slice of `[ui].framework`)
