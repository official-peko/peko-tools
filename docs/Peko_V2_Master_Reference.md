# Peko V2 Master Reference

The single aligned specification for the V2 evolution of Peko: the web platform,
the toolchain and packaging system, building with existing UI frameworks, and the
language and standard library. This file supersedes the older split context files
and the standalone format/OOP drafts wherever they disagree.

---

## 0. About this document

### 0.1 Status and precedence

V2 is the **target state**: the next evolution of Peko, not a description of what
ships today. Where this document and an older draft conflict, the order of authority is:

1. `Peko_V2_Additions.md` (the V2 intent) and the decisions recorded here.
2. `pekoscript-oop-model.md` for object/trait/generic runtime shape.
3. `peko-package-format.md` for container and registry mechanics.
4. The two development-context files (kept out of this synthesis on purpose; they
   described V1 application and authoring detail and were used only as background).

The garbage-collector and C-FFI correctness rules (handles, pinning,
`pgc_begin_blocking`/`pgc_end_blocking`, atomic-vs-malloc, traced-vs-untraced)
remain in force unchanged. V2 only renames the boundary types (`Pointer<T>` becomes
`pointer<T>`); every rule about what may move, what must be pinned, and what must be
bracketed still holds.

### 0.2 Rollout priority

The work is sequenced in three waves. This document is organized to match.

1. **Wave 1 - formats and platform.** The new `peko.toml`, the registry and
   toolchain model, and the unified web platform with hosting.
2. **Wave 2 - building with existing UI frameworks.** SSG, SSR, and PekoUI paths,
   including the webview mock-server and AWS-backed hosting.
3. **Wave 3 - language and standard library.** Erased generics, traits, enums,
   `const`, the new optional and cast model, and the restructured `std`.

### 0.3 How decisions are marked

Calls that were open are written inline as:

> **Decision (recommended):** the chosen default, with the reasoning.

Anything still needing your sign-off is collected in Part VII.

---

# Part I - The Web Platform and Hosting (Wave 1)

## 1. Platform unification

Today the web presence is four separate surfaces: `pekoui.com` (marketing),
`docs.pekoui.com`, `biz.pekoui.com`, and `packages.pekoui.com`. V2 collapses docs,
business, and packages into one authenticated application with a single sign-in and
one navigation surface. Marketing stays separate.

The unified app owns:

- Documentation (language reference, PekoUI, standard library, tutorials, blog).
- The package registry (browse, search, view README/LICENSE, publish, manage).
- The business surface (licenses, insider communications, onboarding courses, teams).
- Hosting management (create/link hosted apps, view status, logs, usage, billing).
- The admin surface (docs, announcements, course content, package review, user roles).

One account, one session, role-gated access to each area.

## 2. Domains

> **Decision (confirmed):** a clean control-plane vs data-plane split.
>
> - `pekoui.com` - marketing only, static, cheap, CDN-served. Stays separate.
> - `app.pekoui.com` - the unified authenticated product (docs, packages, business,
>   hosting management, admin). `docs.`, `biz.`, and `packages.` become routes here
>   (`/docs`, `/packages`, ...) with redirects from the old subdomains.
> - `serve.pekoui.com` - the data plane: hosted user sites (`<app>.serve.pekoui.com`)
>   and the Web Bridge relay (`wss://serve.pekoui.com/...`).

`app.` is preferred over `platform.` simply because it is shorter and is the common
convention for a product's signed-in surface.

**Cookie-isolation requirement (important).** Because `serve.pekoui.com` is a
subdomain of `pekoui.com` rather than a separate apex, hosted user sites could read
any cookie scoped to the parent domain. To keep a compromised or malicious hosted
site from reading first-party auth, **auth cookies must be host-only**: scoped to
`app.pekoui.com` exactly, never set with `Domain=.pekoui.com`. With host-only auth
cookies, nothing served from `serve.pekoui.com` (or its sub-subdomains) can read the
session. If full apex isolation is ever wanted instead, a separate registered apex
(the original `pekoapp.com` idea) is the stronger version of this; the subdomain
approach is fine as long as the host-only cookie rule is enforced without exception.

## 3. Authentication and accounts

Current state is email and Google sign-in via Firebase Auth, with an admin group for
you and the team. Because there are effectively no third-party accounts yet, any
account merging (business users, package publishers) is trivial and can be treated as
a clean unification.

V2 account model:

- **Providers:** email/password and Google to start. GitHub is a natural later
  addition given the audience, but is not required for Wave 1.
- **One identity** across docs, packages, business, and hosting. Role claims attach
  to the identity (see Part 5).
- **CLI sessions** are issued against the same identity (see Part 4).

## 4. CLI to platform handshake, projects, and app IDs

This is the connective tissue between the local toolchain and the hosted platform.

### 4.1 Projects are created on the platform

> **Decision (confirmed):** unify creation under one "project" concept on the web
> app. When a user creates a project online they pick a **type**, and the type
> decides what the project is:
>
> - `package` - a publishable library.
> - `app` - a native Peko/PekoUI application (not hosted; built and run locally).
> - `hosted-app` - a web app (SSG or SSR) that the platform serves.
>
> On creation the platform assigns a stable, platform-unique **app ID**
> (project ID). The app ID is **not** the `peko.toml` bundle identifier; it is a
> separate platform-side identifier so that bundle IDs can change without breaking
> the link, and so one account can own many projects.

### 4.2 `peko login`

A device/localhost OAuth flow, the same shape as `gh auth login` or `vercel login`:

1. `peko login` opens the browser to `app.pekoui.com` for Firebase sign-in.
2. On success the platform hands a token back to a localhost callback the CLI is
   listening on (fallback: a device code the user pastes).
3. The CLI stores the token in the OS keychain (via the platform's secure-storage
   path), never in a plaintext dotfile.

### 4.2.1 Platform interface for `peko login` — IMPLEMENTED (build the CLI against this)

> **Status:** the web/platform half of this flow is implemented in `apps/platform`
> (routes under `/cli/*` and `/api/cli/*`). The CLI implements the local half against
> the HTTP contract below. Base URL is configurable; default `https://app.pekoui.com`
> (dev: whatever port `apps/platform` serves on).

**Model.** Localhost-callback flow, like `gh auth login`. No token ever appears in a
browser-visible URL: the platform hands the CLI a single-use **code**, which the CLI
redeems **server-to-server** for a Firebase **custom token**; the CLI then establishes
its *own* Firebase session (its own refresh token) via `signInWithCustomToken`.

**CLI algorithm.**

1. Start a loopback HTTP server on `127.0.0.1:<port>` (ephemeral port). Generate a
   random `state` (≥ 8 chars, for CSRF).
2. Open the browser to `GET {BASE}/cli/auth?port=<port>&state=<state>`.
3. The user signs in (if needed) and clicks **Authorize**. The platform redirects the
   browser to the loopback server:
   `GET http://127.0.0.1:<port>/callback?code=<code>&state=<state>`
   (or `…?error=access_denied&state=<state>` on cancel). The CLI verifies `state`
   matches, then serves a small "you can close this tab" page.
4. Redeem the code, server-to-server:
   `POST {BASE}/api/cli/token` — body `{ "code": "<code>" }`
   → `200 { "customToken", "uid", "apiKey", "projectId" }`.
   Single use; expires in 5 minutes; `400` if invalid/expired/reused.
5. Establish the CLI's own Firebase session:
   `POST https://identitytoolkit.googleapis.com/v1/accounts:signInWithCustomToken?key=<apiKey>`
   — body `{ "token": "<customToken>", "returnSecureToken": true }`
   → `{ "idToken", "refreshToken", … }`.
6. Store `refreshToken` (+ `uid`, `apiKey`) in the OS keychain — never a plaintext
   dotfile.

**Authenticating later calls (`peko publish` / `peko deploy`).**

- Exchange the refresh token for a fresh ID token:
  `POST https://securetoken.googleapis.com/v1/token?key=<apiKey>`
  — form body `grant_type=refresh_token&refresh_token=<refreshToken>` → `{ "id_token" }`.
- Send it as `Authorization: Bearer <id_token>` on platform API routes. The platform
  verifies it (Firebase Admin `verifyIdToken`) and authorizes by role / tier / ownership.
- Verify a token / read identity: `GET {BASE}/api/cli/whoami` with the Bearer header
  → `200 { "user": { uid, email, displayName, photoURL, role, tier } }`, else `401`.

**Platform endpoints (implemented).**

| Method · Path | Auth | Purpose |
|---|---|---|
| `GET /cli/auth?port&state` | session (sign-in) | Consent screen; on Authorize mints a code and redirects to the loopback callback. |
| `POST /api/cli/authorize` | session cookie | Used by the consent page; mints the custom token + one-time code. |
| `POST /api/cli/token` | none | Redeem code → `{ customToken, uid, apiKey, projectId }` (single use). |
| `GET /api/cli/config` | none | `{ apiKey, projectId }` — if the CLI hasn't cached the API key. |
| `GET /api/cli/whoami` | Bearer ID token | `{ user }` — verify a token and read identity/role/tier. |

**Notes.** The custom token carries a `cli: true` claim so the platform can distinguish
CLI sessions. One-time codes live in Firestore `cliAuthCodes/*` (Admin-SDK only,
5-minute TTL, single use). The Web API key is public by design (it also ships in the
browser client config). `peko link` (§4.3) and `peko publish` / `peko deploy` (§4.4,
Part II) build on the Bearer path above.

### 4.3 `peko link`

Inside a project directory, `peko link` binds the local project to a platform app ID:

- The CLI authenticates with the stored token, lists the account's projects of the
  matching type, and writes the chosen app ID into the project (in `peko.toml` under
  `[project]`, or in `.peko/link` for package projects).
- From then on, `peko publish` (packages) and `peko deploy` (hosting) carry the token
  plus app ID, and the platform authorizes by role and ownership.

### 4.4 What the token authorizes

A single session token, scoped by the account's roles, authorizes: publishing
packages the account owns, creating and deploying hosted apps the account owns, and
reading the account's own dashboards. Admin-only actions require the admin role and
are never available to ordinary tokens.

## 5. Roles and the admin surface

### 5.1 Roles

| Role | Can do |
|---|---|
| Anonymous | Browse docs and public package listings. |
| User (authenticated) | Own projects, publish packages (subject to review), create hosted apps, manage own billing. |
| Business / team member | The above plus team/license/course access for their org. |
| Admin (you and the team) | Everything below. |

Package publishing is open to any authenticated user but flows through admin review
before an entry appears on the public index (matching the current review posture).

### 5.2 Admin surface (enumerated)

The admin area, gated to the admin role, must support:

- **Docs management:** create, edit, version, and publish documentation pages
  (language reference, PekoUI, standard library), and tutorials/blog posts.
- **Announcements:** post and schedule site-wide announcements and the banner.
- **Onboarding courses:** create and edit the business onboarding course structure
  and per-course content, manage video uploads, quizzes, and recaps.
- **Package review:** view submitted packages, inspect manifest and contents,
  approve/reject, and yank published versions.
- **User and role management:** view accounts, grant/revoke roles (business, admin),
  handle team membership.
- **Hosting oversight:** view hosted apps across accounts, status and resource usage,
  suspend abusive deployments.
- **Registry operations:** reserved-name list, name-collision review, takedowns.

## 6. Hosting: the three paths

A hosted project takes one of three forms. The CLI detects the system and handles
setup during build.

1. **Third-party SSG** (React, Vue, SvelteKit static, etc.) - the framework emits
   static HTML/CSS/JS. No account strictly required to build, but hosting the output
   on the platform uses an account.
2. **Third-party SSR** (Django, Flask, Next SSR, etc.) - a long-running server
   process. **Requires a Platform account** because the platform runs the server for
   the user (see Part 18).
3. **PekoUI** - the native framework; built and run locally, optionally using the
   relay for the web view layer.

How SSG and SSR concretely work is in Part III (Wave 2).

## 7. Payments and legal (California LLC)

> This section is informational, not legal or financial advice. The tax, terms, and
> content-liability items below should be reviewed by a CPA and a lawyer before
> hosting goes live, because they turn on facts specific to Peko UI Technologies LLC.

### 7.1 Payment processor

Hosting is a paid feature with a large free tier and pay-as-you-go above it. The core
decision is **Merchant of Record (MoR) vs direct processor.**

- With a **direct processor (Stripe)** you are the merchant of record: you are the
  legal seller and you must register, collect, and remit sales tax/VAT/GST in every
  jurisdiction where you have nexus. Stripe Tax helps calculate it but the filing and
  liability stay with you.
- With a **Merchant of Record (Paddle, Lemon Squeezy, Polar)** the MoR is the legal
  seller. They collect and remit tax worldwide, absorb chargeback handling, and pay
  you a net amount. Typical MoR fee is around 5% + $0.50 per transaction versus
  Stripe's 2.9% + $0.30.

> **Decision (confirmed direction, processor TBD):** use a **Merchant of Record** for
> launch. For a solo
> California LLC selling a developer product globally, the few extra percent buys away
> the entire multi-jurisdiction tax-registration-and-filing burden, which is the
> single largest hidden cost for a one-person company. Concretely:
>
> - **Lemon Squeezy** - simplest setup, indie-focused, now Stripe-owned; good under
>   roughly $250k-$500k ARR. Fastest path to "accepting payments."
> - **Paddle** - widest jurisdiction coverage and the most mature MoR tax stack; the
>   better long-run choice if hosting revenue scales or you sell into the EU heavily.
> - **Polar** - developer-tools-native MoR on Stripe rails; worth watching, newer.
>
> Note Stripe now has its own MoR offering in addition to plain Stripe, at a higher
> effective rate (~3.5% surcharge over standard fees). If you ever want full billing
> control and have the engineering time, Stripe + Stripe Tax is the fallback, but it
> reintroduces the filing burden.
>
> Your Wells Fargo business checking is the payout destination in any of these; the
> MoR or Stripe deposits net revenue there. Keep a separate reserve for taxes the MoR
> does not cover (US federal/state income tax on profit is always yours regardless of
> MoR).

### 7.2 Legal changes to plan for (review with a lawyer)

Because the platform will host user content and run user code, the following are
load-bearing, not optional boilerplate:

- **Terms of Service** for the platform and a separate **Hosting Terms / Acceptable
  Use Policy (AUP).** The AUP is the document that lets you suspend abusive or illegal
  hosted apps and is essential when you run other people's servers.
- **Privacy Policy** compliant with **CCPA/CPRA** (you are a California business
  collecting personal data). Cover what you collect, why, retention, and user rights.
- **DMCA designation.** Register a DMCA agent and publish a takedown process; this is
  what gives a host safe-harbor protection for user-uploaded/served content.
- **PCI scope.** Using an MoR or Stripe keeps card data off your servers, which keeps
  you in the lightest PCI tier. Do not handle raw card numbers yourself.
- **Sales-tax nexus** review with a CPA. An MoR removes most of this, but confirm
  your own US income-tax and any California-specific obligations.
- **Subprocessor disclosure.** If you run hosting on AWS, list AWS (and the MoR,
  Firebase, Cloudflare) as subprocessors in the privacy policy.

## 8. Web stack and infrastructure

Current stack: SvelteKit + Firebase, with the marketing site on `adapter-static` and
Cloudflare Stream used for course video.

> **Decision (confirmed):** split the SvelteKit build by surface.
>
> - **Marketing (`pekoui.com`):** keep `adapter-static`. It is content, it should be
>   cheap and edge-served.
> - **Unified app (`app.pekoui.com`):** move to a **server adapter** (`adapter-node`
>   in a container on Cloud Run, fronted by Firebase Hosting/CDN), because it needs
>   authenticated server routes, the registry API, and hosting-orchestration
>   endpoints. Firebase Auth + Firestore back identity and data. Cloudflare R2 + a
>   static JSON-lines index serve package blobs (Part 12).
> - **Hosting orchestration:** a dedicated backend service (see Part 18) that talks to
>   AWS. Keep it separate from the SvelteKit app so a deploy workload cannot stall the
>   web UI.
>
> Net: Firebase for identity/data/first-party app, Cloudflare for the package CDN and
> video, AWS for running user SSR apps. This is three providers doing what each is
> best at; the alternative (forcing everything onto one) costs more than it saves.

## 9. Marketing site rebuild

A rebuild of `pekoui.com` to feel distinctive rather than templated is in scope as a
goal but is a separate design task; this document only references it. Anchor it to a
real brand direction (the existing Peko identity), keep it a standalone
`adapter-static` deploy, and treat "less AI/cookie-cutter" as a design brief handled
on its own.

---

# Part II - Packaging, Registry, and Toolchains (Wave 1)

## 10. `peko.toml`: the unified manifest

`peko.toml` is the one manifest for everything. It replaces `config.pkbin` (the old
binary app config), `Package.json`, and the `.pkpkg` notion. A directory may be an
app, a library, or both; identity comes from which tables are present.

- `[project]` and `[ui]` - present for applications.
- `[package]` and `[lib]` - present for publishable libraries.
- `[dependencies]`, `[platforms]`, `[native]` - shared.

### 10.1 Library manifest

```toml
[package]
name = "sockets"
version = "1.4.2"
description = "Cross-platform TCP, UDP, and TLS sockets."
license = "MIT"
authors = ["Preston"]
repository = "https://github.com/pekoui/sockets"
keywords = ["net", "tcp", "tls"]
categories = ["networking"]
peko = ">=0.8.0"

[lib]
root = "src/lib.peko"

[dependencies]
json = "^1.2"
collections = { version = "^2.0" }
util = { path = "../util" }            # local path dependency

[platforms]
supported = ["macos", "linux", "windows", "ios", "android"]

[native]
sources = ["c/sockets.c", "c/tls.c"]
include = ["c/include"]

[native.flags]
all = ["-O2"]
linux = ["-D_GNU_SOURCE"]

[native.link]
linux = ["-lpthread"]
macos = ["-framework", "Security"]
windows = ["ws2_32"]

[[native.vendor]]
name = "bearssl"
path = "c/vendor/bearssl"
flags = ["-DBR_NO_SIMD"]
```

### 10.2 Application manifest

```toml
[project]
name = "Counter"
bundle = "com.preston.counter"
version = "0.1.0"
app_id = "pk_app_8fK2..."            # written by `peko link`, platform-assigned
target_platforms = ["macos", "linux", "windows"]

[ui]
framework = "pekoui"                  # one of: pekoui | ssg | ssr
# framework = "ssg"  (react|vue|sveltekit-static|...)
# framework = "ssr"  (django|flask|next-ssr|...)  -> requires a Platform account

[dependencies]
# app dependencies, same rules as a library
```

The `framework` identifier in `[ui]` selects which build path runs (Part 16).

### 10.3 What the manifest does and does not own

The manifest describes identity, dependencies, platform targets, and the native
build (which C files to compile, with what flags, linking what). It does **not**
encode GC/FFI correctness; that lives in source. Source files are discovered by
walking `module::` imports from the root, so adding a `.peko` file never edits
`peko.toml`.

> **Decision (recommended):** require a `src/` directory; `[lib].root` defaults to
> `src/lib.peko`, app entry defaults to `src/main.peko`. A uniform layout keeps the
> packer, resolver, and tooling simple.

## 11. Dependency model and resolution

### 11.1 Dependency kinds

> **Decision (confirmed):** support exactly two dependency kinds:
>
> - **Registry version:** `json = "^1.2"` resolved through the Peko registry.
> - **Local path:** `util = { path = "../util" }` for in-tree development.
>
> There is no git dependency kind. Git is not part of the system at all in V2:
> packages are published and distributed only as `.pkpkg` bundles through the
> registry (Part 12).

### 11.2 What V2 adds over V1

- `peko.toml` always lists dependencies; they are installed automatically into the
  project on build/resolve.
- Dependencies carry **platform targets** and are **built for the project's targets
  at install time** (build-on-install), not shipped as prebuilt per-target binaries.
- `std` is always present at the global root (Part 12.4) and is not listed per
  project as an ordinary dependency.

### 11.3 Resolution and the lockfile

Resolution reads only index metadata and selects exact versions; it never downloads a
package body to learn its dependencies (the index duplicates each version's dependency
list for this reason).

> **Decision (confirmed):** keep a `peko.lock`. It pins, per resolved package, the
> exact version and the `.pkpkg` checksum. This is what makes a second machine and CI
> reproducible and makes a swapped artifact fail the hash check at download. The cost
> is one generated file; the value is reproducibility and tamper-evidence.

## 12. Distribution, registry, and caching

### 12.1 The chosen model

> **Decision (confirmed):** a container-on-CDN registry. Git is not part of the
> system.
>
> - A published version is an immutable, compressed **`.pkpkg` source bundle**
>   (`zstd(tar(flat_project))` behind a small binary header) holding the package's
>   source tree and `peko.toml`. No version is baked inside the tar; the version lives
>   in the storage key and the local unpack path.
> - Bodies live as one immutable object per version in **Cloudflare R2**.
> - A **static JSON-lines index** behind the CDN holds one line per version with the
>   dependency list, checksum, minimum compiler, platforms, and a `yanked`
>   flag. Resolution reads only the index.
> - On install the client downloads the one `.pkpkg`, verifies its checksum against
>   the index line, unpacks it, and **builds it for the project's targets.**
>
> Distribution is entirely through immutable, checksummed `.pkpkg` blobs plus the
> static index; there is no clone step and no dependency on any external code host.

### 12.2 Container header (unchanged from the format draft)

Fixed 32-byte little-endian header: magic `PEKO`, `container_version` (u16),
`compression` (u8), `flags` (u8, bit0 = signed), `meta_len` (u32), `payload_len`
(u64), 12 reserved bytes. Then the embedded `peko.toml` (verbatim UTF-8), then
`zstd(tar(project))`, then an optional detached signature trailer. The embedded
manifest is for instant inspection/validation; the index, not the blob, is
authoritative for resolution.

### 12.3 Caching layout

> **Decision (confirmed):** split global source cache from per-project build cache.
>
> - **Global** (`~/.Peko/registry/`): verified `.pkpkg` blobs in `cache/`, unpacked
>   frozen source trees in `src/<name>/<name>-<version>/`, cached index files with
>   ETags in `index/`. Versions coexist because the version is in the path.
> - **Per-project** (`.peko/`): the per-target built artifacts for the project's
>   declared platforms. Rebuilt when the target set, toolchain, or compiler version
>   changes. Simple and trivially cleanable.
> - A global content-addressed build cache keyed by
>   `(package, version, target, toolchain, compiler)` is a later
>   optimization, not a Wave 1 requirement.

### 12.4 `std` placement

`std` is a normal `peko.toml` package, but it is installed once at the **global root**
during toolchain setup and is always available, so projects do not list it as an
ordinary dependency. Its version is tied to the installed toolchain/compiler.

### 12.5 Publishing

> **Decision (confirmed):** publishing in V2 is authenticated and CLI-driven, with
> web upload remaining as an alternative.
>
> - `peko publish` (authenticated via `peko login` + the project's app ID) packs the
>   `.pkpkg`, uploads it to R2, and a server validates the manifest, appends the index
>   line, and mirrors a row to Firestore for search/discovery. Admin review still
>   gates appearance on the public index.
> - Web upload: a publisher may drag/select-upload a locally built `.pkpkg` in the web
>   app as an alternative to `peko publish`.
> - Yank flips `yanked` in the index line and deletes nothing; existing lockfiles may
>   still depend on a yanked version.

### 12.5.1 Platform interface for `peko publish` — IMPLEMENTED (build the CLI against this)

> **Status:** implemented in `apps/platform`. Uploads stage privately in Firebase
> Storage; on **admin approve** the server writes the immutable blob and the index
> line to R2 (`packages/<name>/<name>-<version>.pkpkg`, `index/<name>`) and mirrors a
> row to Firestore. The CLI implements the client half below. Auth is the Bearer ID
> token from `peko login` (§4.2.1); web upload uses the session cookie instead — same
> endpoints.

**`peko publish` algorithm.**

1. Build the `.pkpkg` for the current package version (§12.2 container).
2. `POST {BASE}/api/publish/start` — header `Authorization: Bearer <id_token>`, no body.
   → `200 { "requestId", "uploadUrl", "storagePath" }`.
3. Upload the raw `.pkpkg` bytes:
   `PUT <uploadUrl>` with header `Content-Type: application/octet-stream` and the file
   as the body. (`uploadUrl` is a presigned Firebase Storage URL, ~15-min TTL.)
4. `POST {BASE}/api/publish/complete` — Bearer — body `{ "requestId": "<id>" }`.
   The server downloads the staged blob, parses the §12.2 header + embedded
   `peko.toml`, enforces the name rules (§12.6) and namespace ownership, rejects a
   duplicate version, and queues it for review.
   → `200 { "status": "pending", "name", "version" }`, or `400 { "error" }`.
5. The version is now **pending admin review**; it appears on the public index only
   after an admin approves it in the platform dashboard.

**Notes.** Name/version/deps/platforms/min-compiler are read from the embedded
`peko.toml` — the CLI does not send them separately. Max size 50 MB. The server
computes the checksum (SHA-256 of the whole `.pkpkg`) itself. Web upload
(`/packages/publish`) drives the exact same `start` → upload → `complete` flow with a
session cookie instead of a Bearer token.

### 12.6 Name rules (enforced server-side)

ASCII only; alphanumeric plus `-` and `_`; first character alphabetic;
case-insensitive collision detection with `-` and `_` treated as colliding; bounded
length; reserved names rejected.

### 12.7 Registry server contract

This is the normative contract between the CLI client (`crates/peko-cli/src/registry`)
and the registry backend. The client reads a static, cacheable surface; publishing is
the only write path and is authenticated. Two invariants govern everything below:

- **The index is authoritative for resolution.** Resolution reads only the index and
  never downloads a body to learn its dependencies. The blob's embedded `peko.toml`
  (Section 12.2) exists for inspection and publish-time validation only; the server
  derives the index fields from it at publish and thereafter the index is the source
  of truth.
- **Published versions are immutable.** A `(name, version)` blob and its checksum
  never change once accepted. Re-publishing the same version is rejected. This is what
  makes `peko.lock` checksums durable and tamper-evident.

The client's base URL is `PEKO_REGISTRY_URL` (a placeholder until the platform is
live). All paths below are relative to it.

#### 12.7.1 Read endpoints

The read surface is static files behind the CDN; the server never sees a resolution.

| Request | Path | Success | Not found | Notes |
|---|---|---|---|---|
| Index | `GET /index/{name}.jsonl` | `200` JSON-lines body | `404` | one line per version; see 12.7.2 |
| Blob | `GET /blobs/{name}/{name}-{version}.pkpkg` | `200` container bytes | `404` | immutable `.pkpkg` per 12.2 |

- The index response is `application/x-ndjson` (or `text/plain`); the client parses
  the whole body line by line and ignores blank lines.
- `404` on the index means the package is unknown and is surfaced as such; any other
  non-`2xx` is an error. A network failure (not an HTTP status) falls back to the
  client's cached copy of the index, so the server should return real `404`s rather
  than a `200` empty body for unknown packages.
- The index SHOULD carry an `ETag`; the client SHOULD send `If-None-Match` and honor
  `304 Not Modified` against its cached copy (Section 12.3). Blobs are immutable and
  SHOULD be served with a long-lived `Cache-Control: public, max-age=31536000,
  immutable`.
- The blob body is verified against the index `checksum` after download; a mismatch is
  a hard error, so a byte-exact object per key is required.

#### 12.7.2 The index line

One JSON object per line, minified, newline-terminated. Field names are exact (the
client deserializes into `IndexEntry`).

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `name` | string | yes | - | package name (Section 12.6 rules) |
| `version` | string | yes | - | exact semver of this version |
| `deps` | object&lt;string,string&gt; | no | `{}` | dependency name to version requirement |
| `checksum` | string | yes | - | `sha256:<hex>` over the whole `.pkpkg` file |
| `min_compiler` | string | no | omitted | minimum compiler semver requirement |
| `platforms` | array&lt;string&gt; | no | `[]` | supported OS ids: `macos`, `linux`, `windows`, `ios`, `android` |
| `yanked` | bool | no | `false` | withdrawn from new resolutions |

```json
{"name":"std","version":"0.1.0","deps":{"json":"^1.2"},"checksum":"sha256:802a26...","min_compiler":"^1.0","platforms":["macos","linux"],"yanked":false}
```

Order within a file is not significant to the client (it reads every line), but
ascending by version is recommended. `deps` and `platforms` are absent-as-empty;
`min_compiler` is omitted entirely when the package sets no floor.

#### 12.7.3 Publishing

Publishing is authenticated (`peko login` plus the project's app id) and CLI-driven,
with web upload as an alternative (Section 12.5). The client packs the `.pkpkg` and
uploads the bytes; the server is responsible for validating and admitting it.

On receiving an upload the server MUST, and reject the publish on any failure:

1. Parse the container header and the embedded `peko.toml` (Section 12.2). The
   `peko verify` pass (`registry::verify`) is the reference validator: header and
   framing are well-formed, the manifest is a `[package]` with `[lib]`, the packed
   `peko.toml` matches the embedded one, and the `[lib].root` entry file is present.
2. Enforce the name rules (Section 12.6) and that the semver `version` parses.
3. Reject path dependencies; a published package's `deps` are registry requirements
   only.
4. Recompute `sha256:<hex>` over the received bytes; this becomes the index
   `checksum`.
5. Reject when `(name, version)` is already published (immutability).

On success the server: stores the blob write-once at `blobs/{name}/{name}-{version}.pkpkg`;
derives `deps`, `min_compiler`, and `platforms` from the embedded manifest and appends
one index line (12.7.2) to `index/{name}.jsonl`; and mirrors a search row (Firestore)
for discovery. Admin review still gates appearance on the public index.

#### 12.7.4 Yank

Yank is an authenticated metadata flip: set `yanked` to `true` on the version's index
line and delete nothing. Resolution avoids selecting a yanked version for a new
requirement, but an existing `peko.lock` may still name one, and downloading it by its
pinned checksum continues to work.

## 13. Toolchains (pluggable)

A toolchain is what already exists in V1 (the libs and includes for building C/C++)
but is now **pluggable** and self-describing.

- Each toolchain ships a `toolchain.toml` that defines its exact target and how to use
  it: include paths, library paths, linker flags, how to compile C/C++/Objective-C
  against it, and which dynamic libraries must be bundled into the final app.
- Toolchains are **opt-in.** Users install only the targets they need. **Apple targets
  have special setup** (Apple SDK acquisition and signing prerequisites).
- All toolchains are **vendored by the Peko team** for now. The pluggable shape exists
  so future targets (for example a TV or smart-watch OS) can be added without changing
  the compiler.

Conceptual `toolchain.toml` shape:

```toml
[toolchain]
id = "macos-arm"
target = "macos"
arch = "arm"

[build]
# how to compile C / C++ / Objective-C for this target
c_flags = ["..."]
cxx_std = "c++17"
objc = ["-x", "objective-c", "-fobjc-arc"]
include = ["..."]

[link]
lib_paths = ["..."]
flags = ["..."]
bundle_dylibs = ["..."]      # dynamic libs copied into the app bundle
```

The CLI reads the active toolchain for the current build target to drive C
compilation and linking (Part 14, Part 15).

## 14. C interop

### 14.1 Two coexisting boundaries

V2 keeps the `[external]` Peko-side declaration **and** adds a header-driven model.
Both coexist on purpose: `[external]` for libraries that cannot use the Peko build
system, and `.peko.h` for sources the Peko compiler builds itself.

### 14.2 The `.peko.h` model

A `.peko.h` file is valid to the C preprocessor **and** parseable by Peko. It is one
file with two readers: the C compiler sees ordinary C declarations, and the Peko FFI
parser reads the same file unpreprocessed to pull out the declarations that cross into
Peko.

The shared vocabulary is a set of macros defined in `peko.h`. Each one expands to a
concrete C type or to nothing, so the C compiler sees plain C, while the Peko FFI
parser maps the same tokens to V2 FFI types.

`source/file.peko.h`:

```c
#include <peko.h>

PEKO_BEGIN

p_fn p_gc_opaque mem_alloc(p_i32 bytes);
p_fn p_gcsafe void run_callback(p_gc(Buffer) handle);
p_fn p_i32 printf(p_cstr fmt, ...);
p_var p_i64 frame_count;

PEKO_END
```

The vocabulary:

- **`#include <peko.h>`** brings in the `p_*` macros. The compiler installs it at
  `<install>/Compiler/include/peko.h`.
- **`p_fn`** marks a function declaration, **`p_var`** marks a variable declaration.
  Both lead the declaration, expand to nothing for the C compiler, and are read by the
  Peko FFI parser. An unmarked declaration is C-only and is not imported.
- **Types** use the `p_*` aliases. Scalars: `p_ch`, `p_i16`, `p_i32`, `p_i64`,
  `p_i128`, `p_f16`, `p_f32`, `p_f64`, `p_bool`. Pointers cross the boundary only
  through `p_gc(T)` and `p_gc_opaque` (GC-managed, traced and movable), `p_opaque`
  (an unmanaged `malloc` or OS handle), or `p_cstr` (a C string). A raw `*` is rejected.
- **`p_gcsafe`** sits before the return type and marks a function whose call is a GC
  safepoint, for a C function that can collect, block, or call back into Peko. An
  unmarked FFI function is a leaf and must not collect.
- **`...`** marks a C variadic.
- **`PEKO_BEGIN` / `PEKO_END`** wrap the declarations so a C++ compiler gives them C
  linkage. They expand to nothing in C and are ignored by the Peko FFI parser.

The `p_*` alias to Peko FFI type mapping:

| Alias | Peko FFI type |
|---|---|
| `p_ch` | `char` |
| `p_i16` `p_i32` `p_i64` `p_i128` | `i16` `i32` `i64` `i128` |
| `p_f16` `p_f32` `p_f64` | `f16` `f32` `f64` |
| `p_bool` | `bool` |
| `p_cstr` | `cstr` |
| `p_opaque` | `opaque` |
| `p_gc_opaque` | `pointer<void>` |
| `p_gc(T)` | `pointer<T>` |

`source/file.c` and `source/file.m`:

```c
#include "file.peko.h"
// implementations
```

`file.peko`:

```peko
import source::file;                  // parses file.peko.h; imports its functions
link libs::OS::ARCH::lib as lib;      // OS and ARCH resolve to the active build target
linker "-L this";                     // pass a linker argument directly

fn OnStart() {
    file::mem_alloc(64);              // call the C symbol by its raw name through the module
}
```

A `p_fn` function imports as an external symbol with its raw C name, so a
`file::symbol` call resolves to the C function directly with no Peko name mangling.

> **Planned (not yet implemented):** a header config block names which C/ObjC files
> implement the header and on which platforms, so the compiler can build and link them
> automatically (Part 14.3). The intended shape is a `defined_in` list, for example
> `defined_in = ["file.c", { "file.m", platforms = ["macos"] }]`. The current parser
> reads only the declarations; this block is the next addition.

### 14.3 What the compiler now does automatically

> **Decision (confirmed):** the Peko compiler **compiles the C/ObjC files named by
> `defined_in` automatically** and links them for the correct platform, using the
> active toolchain's flags. The old manual workflow (`peko clangflags` + a hand-written
> build script placing objects into a `libs/<os>/<arch>/` tree) is retired for these.
> `link libs::OS::ARCH::...` is **reserved for prebuilt or vendored libraries** that
> cannot use the Peko build system. `OS` and `ARCH` segments in a `link` now resolve to
> the current build target. `linker "..."` passes raw arguments to the linker.

## 15. Linker and compiler-driver architecture

> **Decision (confirmed):** Peko LLVM emits **objects only**; the CLI drives
> compilation and linking.
>
> - Embed **LLVM + LLD** into the Peko backend so codegen and linking live in one
>   self-contained binary. LLD is designed to be embedded as a library, and the
>   backend already links LLVM, so this adds little surface.
> - Do **not** embed Clang. Clang's driver is enormous and would bloat the binary by
>   hundreds of megabytes; the toolchain already ships and versions a clang. The CLI
>   invokes the **toolchain's clang** to compile C/C++/Objective-C, then links the
>   resulting objects with embedded LLD.
> - Allow a toolchain to **override the linker** for targets that need the platform's
>   own linker (Apple), via `toolchain.toml`.
>
> Net: one binary owns Peko codegen and the default link step; clang stays external
> and per-toolchain. This gives the "single self-contained binary" feel for everything
> Peko generates, without paying clang's embedding cost.

---

# Part III - Building with Existing UI Frameworks (Wave 2)

## 16. The three UI paths

`[ui].framework` selects one of three build paths, and the CLI detects the system and
handles setup during build:

1. **SSG** - a third-party static generator (React, Vue, SvelteKit static, ...).
2. **SSR** - a third-party server framework (Django, Flask, Next SSR, ...), which
   **requires a Platform account** because the platform runs the server.
3. **PekoUI** - the native framework.

## 17. SSG via the webview mock-server

The Peko app already embeds a webview. For the SSG path:

> **Decision (confirmed):** the framework's static output (HTML, CSS, JS) is built,
> then the **`ui` submodule for interfacing with an existing page** is given access to
> that built source. It stands up a **local mock-server** that serves the built assets
> and renders them through the built-in webview.

So an SSG app ships as: built static assets + a thin Peko host that serves them over
loopback and displays them in the native webview. This reuses the existing
webview/asset-serving machinery (now part of the consolidated `pekoui` package, Part
19) and needs no remote hosting to run locally.

## 18. SSR via hosted AWS

SSR frameworks need a running server, so they are hosted by the platform.

> **Decision (confirmed):** the platform provisions and runs SSR apps on **AWS**,
> auto-linked for authenticated users, with a large free tier and pay-as-you-go above
> it.
>
> Architecture:
>
> - **Deploy:** `peko deploy` (authenticated, carrying the app ID) uploads the build.
>   The orchestration backend builds a **container image**, pushes it to a registry
>   (ECR), and runs it as a managed container service.
> - **Run:** **AWS App Runner** or **ECS Fargate** for SSR servers (managed
>   containers, autoscaling, HTTPS). App Runner is the simplest managed path; Fargate
>   gives more control. For very light endpoints, Lambda + API Gateway can scale to
>   zero, but not every framework fits cleanly, so containers are the default.
> - **Routing and TLS:** each app gets `<app>.serve.pekoui.com` with TLS via ACM,
>   fronted
>   by CloudFront/ALB. Custom domains are a later addition.
> - **Sync:** when a user is authenticated the platform can set up and link hosting
>   automatically, so the path from "I have an SSR project" to "it is live" is one
>   command.
> - **Free vs paid:** the free tier is enforced by resource caps (CPU/memory/requests
>   per app, number of apps). Above the cap, usage is metered and billed pay-as-you-go
>   through the MoR (Part 7). The control plane (Firebase + the orchestration service)
>   holds app metadata and usage; AWS runs the compute.
>
> Be aware this is the heaviest piece of infrastructure in V2: running other people's
> servers means real cost, isolation, abuse handling (the AUP in Part 7), logging, and
> metrics. Treat it as its own project with its own milestones.

## 19. PekoUI package restructure

> **Decision (confirmed):** drop the standalone `ffi` package, and **consolidate
> `pekoui` + `storage` + `webview` + `assets` into one `pekoui` package.** Within it,
> add a **submodule for simple UI that interfaces with an existing page** (the SSG host
> from Part 17). PekoUI remains a **separate package** from `std` and is no longer
> auto-imported by default, since an app now explicitly chooses its UI path.

---

# Part IV - Language V2 (Wave 3)

## 20. Types and the object model

### 20.1 Surface data types are objects

The surface ("data") types are: `number`, `string`, `bool`, `char`, `T[]`
(`== Array<T>`), `T?` (`== Option<T>`), and `{T1: T2}` (`== Map<T1, T2>`). **All map
to objects.** `{T1: T2}` is purely shorthand for a map type; the `#[...]` and `#{...}`
literal shorthands are kept.

- `number` is an **object** (a class instance) created by default whenever a numeric
  literal is written. Application code does numeric work in `number`; integer-vs-float
  representation is the object's concern, not a surface distinction.
- `string` is the single string type. The old `String` class is **collapsed into
  `string`**; all the methods that lived on `String` now belong to `string`.

### 20.2 FFI types

FFI types are the raw, unmanaged, codegen-level types, used only in C-interop code:

`i1, i8, i16, i32, i64, i128`, `f16, f32, f64`, `opaque`, `&T`, `pointer<T>`.

> **Decision (confirmed):** there is no separate `double` FFI type. Floating-point at
> the FFI boundary goes through `f64` (and `f16`/`f32`). `double` is dropped.

FFI constants are made with a built-in:

```peko
constant<i1>(true)
constant<f64>(1.3)
constant<&i8>("ASDF")
```

### 20.3 `pointer<T>`, `&T`, `opaque`

- `pointer<T>` is the GC-traced managed pointer (address space 1). It is the V2
  spelling of the old `Pointer<T>`; every GC rule about it is unchanged.
- `&T` is a reference; at the codegen level it is **not distinct from `opaque`**.
- `opaque` is a non-GC, stable C/OS handle.

Because every value is now an object (and therefore already a heap reference), `&T`
and `opaque` are needed essentially **only in FFI code**. Ordinary application code
does not reach for them.

## 21. Variables, `const`, and casting

### 21.1 Declarations

Variables are declared with `let`:

```peko
let x: number = 42;
let name: string = "Preston";
let items: number[] = #[1, 2, 3];
```

### 21.2 `const` as a type modifier

`const` is a type modifier: `const T`. A `T` value converts **to** `const T`
automatically, but not the reverse. You cannot reassign through a `const` binding,
cannot reassign an attribute on a `const` value, and cannot call a `[mutates]` method
on a `const` value. Casting away `const` is an explicit `as`:

```peko
let name: const string = "Preston";
name.push('!');                 // does not compile: push mutates
let name_mut: string = name as string;   // explicit const-to-mutable cast
name_mut.push('!');             // fine
```

### 21.3 The cast model: `as` and `danger_cast`

Implicit casts are gone. **Everything is explicit**, through one of two forms:

| Form | Meaning | Fails how |
|---|---|---|
| `value as T` | Statically-proven-safe cast: `const`-to-mutable, object to a related object, concrete to a trait the static type already carries. | Compile error if not provably safe. |
| `danger_cast<T>(value)` | Unchecked, FFI-level force: pointer-to-pointer, int-to-float, managed-to-unmanaged, any object to any object. | No check; undefined if wrong. |

Notes:

- There is **no `as?`** and no runtime-checked cast built-in; a cast is either
  statically safe (`as`) or a forced `danger_cast`.
- **Managed-to-unmanaged** casts (object or `pointer<T>` to `&T` or `opaque`) are
  **never** allowed through `as`; they require `danger_cast`.
- `danger_cast` is also the only way to cast between unrelated object types and to
  cast one enum type to another (Part 23.4).

### 21.4 Casting to a trait

> **Decision (confirmed):** a value may be cast to a trait with `value as Trait` only
> when its **static type already carries the trait**. The witness is known at compile
> time, so it builds the fat-pointer trait object with no possibility of failure. When
> the static type does not carry the trait, the cast is not expressible through `as`;
> the escape hatch is `danger_cast<Trait>(value)`, which forces it with no runtime
> check. (A runtime-checked fallible trait cast was considered and dropped; it can be
> reintroduced later as a built-in returning `Trait?` if a real need appears.)

## 22. Optionals and error context

`Option<T>` (`T?`) still has three states: **value**, **None**, and **Error**. V2 adds
origin context.

- Both `None` and `Error` carry **where they originated** (file, line, character), so
  the runtime can print a meaningful location when an unwrap halts or an error is
  reported.
- `?` now **propagates** in functions that return an optional, and **halts** in
  functions that do not (see 25.2).
- `expr? else { ... }` overrides the failure branch and may **yield a fallback value**.

## 23. OOP: generics, traits, enums, instantiation

### 23.1 Erased generics

A generic type or function compiles **once**. Every type parameter `T` is one
`Object*`, so layout and body do not depend on the concrete argument
(`Box<number>` and `Box<string>` share one compiled struct and method set). This is
type erasure, not monomorphization; monomorphization may be layered on later as a
transparent speed optimization.

To call methods on a `T`, constrain it:

```peko
class Sorter<T: impl Comparable> { ... }
fn rename<T: from Animal>(x: T, n: string) { ... }
fn f<T: impl Drawable, from Widget, impl Serialize>(t: T) { ... }   // stacked
```

- `from CLASS` grants field and method access through the object's own descriptor.
  Free at runtime (fixed offsets and vtable slots).
- `impl TRAIT` grants the trait's methods, resolved through the object's runtime type.
  No hidden dictionary argument is threaded through generic functions.

### 23.2 Traits

```peko
trait Speak {
    fn speak() => string;          // no body: implementer must provide it
}

trait Container<T> {
    fn get(i: number) => T;
    fn put(i: number, v: T);
}
```

- A trait is a set of method slots. A slot with **no body** is an override the
  implementer must supply; a slot **with a body** is a default the implementer may keep
  or replace.
- Each class that implements a trait produces a fully populated **witness table**.
  Trait dispatch is **virtual**: it always resolves through the object's runtime type,
  giving Java-style interface semantics (a value upcast to a parent still runs the
  most-derived implementation).
- Generic traits erase: `Container<number>` and `Container<string>` share one witness
  shape; the type arguments do compile-time checking only.
- **Coherence:** a type implements a given trait at most once; the same trait at two
  different instantiations on one type is not supported.

### 23.3 Object instantiation

Objects are now instantiated with `new`:

```peko
let s = new Sorter<number>();
let d = new Dog("Rex");
```

### 23.4 Enums

Enums are identifier-only and integer-backed, but they **behave like types**:

```peko
enum Direction {
    North,
    East,
    South,
    West
}
```

- You **cannot** cast one enum to another through `as`; that requires `danger_cast`.
- A V2 **`switch` over enums**, in the shape of Rust's `match`, is added for exhaustive
  handling. The keyword is **`switch`**. A switch must cover every variant, or include
  a **`_ =>` default arm** that catches the rest:

```peko
switch dir {
    Direction::North => { ... }
    Direction::East  => { ... }
    Direction::South => { ... }
    Direction::West  => { ... }
}

switch dir {
    Direction::North => { ... }
    _ => { ... }                 // catches every remaining variant
}
```

A `switch` with neither full coverage nor a `_` arm is a compile error.

## 24. Usage tracking, mutation tracking, and `on_state_changed`

### 24.1 Usage tracking and `[public]`

- A function, variable, class, attribute, or method that is actually used is marked
  used; **unused ones produce a warning.**
- `[public]` is **automatic**; `[private]` is **explicit.** Marking something
  `[public]` explicitly **suppresses the unused warning** (it declares an intentional
  external surface). `[private]` restricts access to the declaring class/module.

### 24.2 Mutation tracking

- A method is auto-marked `[mutates]` when it **reassigns an attribute** of its class.
- A method is also auto-marked `[mutates]` when it **calls a `[mutates]` method on one
  of its class attributes.**
- `[mutates]` may still be **hand-written.**
- A `[mutates]` method **cannot be called on a `const` value** (ties into 21.2).

### 24.3 `on_state_changed`

`on_state_changed(name: string)` (the rename of the old `onStateChanged`) is an
**overridable method on the root `Object` type**, present on every class. It is
**always called** whenever a class attribute is modified, by reassignment or by a
`[mutates]` method. (It is a root-`Object` method, not a trait.)

## 25. Sugar

### 25.1 `if` as an expression

`if` may stand alone or be part of an assignment or argument. When it is part of an
assignment/argument, the **last expression of each branch** is pulled into the PHI and
becomes the value; every branch must evaluate to the **same type**.

```peko
let sign = if n > 0 { 1 } else if n < 0 { -1 } else { 0 };
```

### 25.2 `?` desugaring

`?` now desugars based on the enclosing function's return type.

In a function returning `T?`, `?` **propagates** (attaching context and returning the
optional):

```peko
let value = expr?;

// becomes:
let value = if expr.is_value() {
    expr.unwrap()
} else {
    expr.add_context(FILE_NAME, LINE, CHARACTER);
    return danger_cast<T?>(expr);
};
```

In a function not returning an optional, `?` **halts**:

```peko
let value = if expr.is_value() {
    expr.unwrap()
} else {
    expr.halt();
};
```

You may override the failure branch, which makes `?` an expression that yields a
fallback. The `else` runs on **both None and Error** and may produce a value:

```peko
let value = expr? else {
    compute_default()
};
```

---

# Part V - Standard Library V2 (Wave 3)

## 26. `std` structure and imports

`std` is **one root package** with submodules:

```
std::
  core
  collections
  io
  sockets
  threads
  xml
  json
  lexer
  fs
  runtime
  crypto
```

Auto-import behavior:

> **Decision (confirmed):**
>
> - `std::core` and `std::collections` are **auto-imported and unpacked** (their names
>   are used bare). Together they are what V1 called `standard`.
> - `std::runtime`, `std::xml`, and `std::json` are **auto-imported but not unpacked**
>   (used through their prefix, e.g. `xml::`, `json::`, `runtime::`).
> - Everything else (`io`, `sockets`, `threads`, `lexer`, `fs`, `crypto`) needs an
>   explicit `import` and a prefix.

Renames from V1:

| V1 | V2 |
|---|---|
| `standard` (bare) | `std::core` + `std::collections` (bare) |
| `console::` | `std::io` |
| `Runtime::` | `std::runtime` |
| `ffi` package | dropped |

`std::io`, `std::crypto`, `std::fs`, `std::lexer`, `std::sockets`, `std::threads` are
ported from the V1 packages with V2 support added.

## 27. `core` and `collections`

### 27.1 `std::core`

`core` holds the root `Object`, `Option`, and the operator/iteration traits.

`class Object` (root) provides at least:

```peko
fn to_string() => string;
fn to_number() => number;
fn on_state_changed(name: string);     // overridable; always called on mutation
```

`class Option<T>`:

```peko
value: pointer<T>;
fn is_none() => bool;
fn is_error() => bool;
fn is_value() => bool;
fn unwrap() => T;
fn unwrap_else(...) => T;
fn unwrap_or_else(...) => T;
// ... plus add_context / halt used by the ? desugaring
```

Core traits (these replace the old `[operator ...]` member syntax entirely):

- Iteration: `Iter` (`next`, `back`, `check`, helpers).
- Comparison: `Equals`, `NotEquals`, `GreaterThan`, `LessThan`, `GreaterThanEquals`,
  `LessThanEquals`.
- Arithmetic: `Plus`, `Minus`, `Multiply`, `Divide`, `Exp`, `Mod`.
- Hashing: `Hash`.

> **Decision (confirmed):** operator overloading is done by **implementing these core
> traits**; the `[operator +]` / `[operator []]` / `[operator iterator]` member syntax
> is **removed**.

### 27.2 `std::collections`

```
Array<T: impl Equals, impl NotEquals>
  push, remove, insert, find, basic array operations.

List<T: impl Equals, impl NotEquals, impl GreaterThan, impl LessThan,
      impl GreaterThanEquals, impl LessThanEquals> from Array<T>
  adds sort variants and richer search.

Map<KT: impl Hash, VT>
  a real hashmap (KT must implement Hash).

IndexMap<KT, VT>
  simple index-based map.

Pair<T1, T2>
  basic pair.
```

The constraint sets above are meaningful: `Map` now requires `KT: impl Hash`, and the
collection element constraints are spelled with `impl TRAIT` bounds.

## 28. `xml`, `json`, and serialization

### 28.1 `std::xml`

Element literals and serialization now go through `std::xml` (the element type is
ported from the old `ui::Element`). This means **element literals and serialization
are usable without PekoUI.** `std::xml` implements serialization; `std::json` is mostly
the same as V1 with serialization added.

### 28.2 `[serial]` classes and the serialize/deserialize built-ins

A `[serial]` class may only have attributes that are data types (including arrays and
maps) and other `[serial]` classes. `[serial]` desugars into generated functions that
walk attributes to and from JSON.

> **Decision (confirmed):** name the built-ins by the universal convention, so they
> read the same as serde / every other ecosystem:
>
> - `serialize(value) => json::Value` - object to JSON.
> - `deserialize<T>(json::Value) => T?` - JSON to object, `None`/`Error` on failure.
>
> Field handling inside a `[serial]` class:
>
> - **Optionals (`T?`):** on deserialize, an **absent key becomes `None`** (not an
>   error); a present-but-wrong-type value is an `Error`. On serialize, a `None`
>   **omits the key** (and a JSON `null` also deserializes to `None`).
> - **Enums:** serialize to their **identifier string** and deserialize from it; an
>   unknown string is an `Error`. Strings are chosen over the backing integer because
>   they survive reordering and are human-readable in the JSON.
> - A missing required (non-optional) attribute on deserialize returns `Error(...)`
>   with the field name.

---

# Part VI - Compiler and Codegen Internals

## 29. The `PekoType` model

`PekoType` moves to an enum form that carries `const`-ness, generics, and restraints:

```rust
struct PekoTypeInfo {
    name: String,
    generics: Option<Vec<PekoType>>,
    array_depth: usize,
    reference_depth: usize,
}

enum TypeRestraint {
    Impl(PekoType),
    From(PekoType),
}

enum PekoType {
    // const, arguments, return, is_closure
    Function(bool, Vec<PekoTypeInfo>, Option<PekoTypeInfo>, bool),
    // const, type
    Basic(bool, PekoTypeInfo),
    // generic name, restraints
    Generic(String, Vec<TypeRestraint>),
}
```

## 30. Codegen and simulator changes

The moves that follow from V2:

- **Symbols track `used`.** The `Execution*` symbol records gain a used flag; the same
  for variables and class attributes (drives the unused warnings in 24.1).
- **`ExecutionClassGeneric` merges into `ExecutionClass`.** Erased generics compile
  once, so the separate generic-class path is no longer needed.
- **Object construction simplifies** in both codegen and simulation (uniform boxed
  representation, `new`-based instantiation).
- **Type overload/match checking simplifies** under explicit casts and erased
  generics.
- **Parser and ASTs change** for `let`, `const T`, `new`, `trait`, `enum`, the enum
  `switch`, `if`-expressions, the new `?`/`else` desugaring, and the `.peko.h`
  config/header grammar.
- **A new `.peko.h` parser** reads the `p_fn` and `p_var` marked declarations and maps
  the `p_*` vocabulary to V2 FFI types. The header config block (Part 14.2) is planned.
- **The linker becomes a separate concern driven by the CLI.** Per Part 15, Peko LLVM
  emits objects only; the CLI compiles C via the toolchain's clang and links with
  embedded LLD (toolchain may override the linker).

The GC integration rules from the object model are unchanged: every `T`-typed field is
one traced `pointer<void>` slot; `TypeInfo`, vtables, super-vtables, and witness tables
are static constant data that must never be GC-allocated or traced; a trait object's
fat pointer traces only its first word (`self`) and never the witness.

---

# Part VII - Decision log and remaining open items

### Resolved

These were open and are now locked into the body of this document:

1. **FFI floats (20.2).** `double` dropped; floats go through `f16`/`f32`/`f64`.
2. **Cast surface (21.3, 21.4).** Two forms only: `as` (statically safe) and
   `danger_cast<T>` (forced). No `try_cast`, no `as?`. Trait casts use `as` when the
   static type carries the trait, else `danger_cast`.
3. **Serialization (28.2).** `serialize(value) => json` (object to json),
   `deserialize<T>(json) => T?` (json to object). Absent optional key becomes `None`;
   enums round-trip as their identifier string.
4. **Enum `switch` (23.4).** Keyword is `switch`; must be exhaustive or include a
   `_ =>` default arm.
5. **Linker (15).** Always embed LLD in the backend; ship clang with the toolchain and
   invoke it separately; toolchain may override the linker (Apple).
6. **Registry (11, 12).** Source bundles renamed `.pkpkg`, distributed as immutable
   blobs on Cloudflare R2 plus a static JSON-lines index. Git is removed from the
   system entirely (no git dependencies, no git publish source).
7. **Lockfile (11.3).** `peko.lock` is kept, pinning version + `.pkpkg` checksum.
8. **Build-cache scope (12.3).** Per-project built artifacts now; global
   content-addressed cache as a later optimization.
9. **Domains (2).** `app.pekoui.com` for the control plane and `serve.pekoui.com` for
   the data plane (hosted sites + relay), with the host-only auth-cookie rule to
   preserve isolation across the shared parent domain.
10. **SvelteKit adapter (8).** Static marketing; `adapter-node` on Cloud Run for
    `app.pekoui.com`.
11. **SSR compute (18).** AWS App Runner / Fargate as the SSR runtime.

### Still open

1. **Payment processor (7.1).** Merchant-of-Record direction is set; the specific
   provider (Lemon Squeezy vs Paddle vs Stripe's own MoR) and the legal items in 7.2
   (ToS, AUP, CCPA privacy, DMCA agent) are deferred and want a CPA and lawyer before
   launch.
2. **`src/` layout (10.3).** Whether package and app source must live under a `src/`
   directory (clean root: `peko.toml`, `README`, `LICENSE`, `c/`, `assets/` at top
   level, code under `src/`, so `src/lib.peko` and `src/main.peko`) or may sit loose at
   the project root. Layout-only; affects the scaffolder and the default entry path.
3. **Free-tier hosting caps (18).** The concrete per-app resource limits (CPU, memory,
   request volume, app count) that define the free tier before pay-as-you-go.
