# Peko SSR Hosting: Architecture, Metering, and Observability

## 0. About this document

### 0.1 Status and precedence

This is the implementation design for Part I section 18 ("SSR via hosted AWS")
and the AWS half of section 8 ("Web stack and infrastructure") of
`Peko_V2_Master_Reference.md`. It extends those sections and does not override
their confirmed decisions. Where the master reference marks a decision confirmed
(AWS as the SSR runtime, App Runner as the default managed path, the control
plane on Firebase, `serve.pekoui.com` as the data-plane parent), that decision
stands here.

What this document adds is the enterprise layer the master reference names but
does not design: full per-account and per-app usage tracking, metrics, billing
attribution, and the user-facing surfaces that expose them. Section 18 lists
"logging, and metrics" and "isolation, abuse handling" as first-class work and
says to treat SSR hosting as its own project with its own milestones. This is
that project's plan.

### 0.2 Design goals

1. Every unit of resource use (compute time, requests, egress bytes, build
   minutes, storage) is attributable to exactly one account and one app, in near
   real time for display and reconciled to authoritative numbers for billing.
2. Users see their own usage, metrics, logs, and cost estimate without operator
   involvement.
3. The free tier is enforced by the same signals that drive billing, so the
   number a user sees and the number they are charged on come from one ledger.
4. AWS runs compute only. Identity, app metadata, and the usage ledger of record
   stay in the control plane, per section 8.
5. Isolation is per app. One tenant cannot read another tenant's traffic, auth,
   logs, or metrics.

## 1. The three planes

The master reference describes a control plane and a data plane. Enterprise
metering makes the metering plane a third first-class concern.

- Control plane. `app.pekoui.com` (SvelteKit `adapter-node` on Cloud Run) plus a
  dedicated orchestration backend. Owns identity (Firebase Auth), app metadata,
  the usage ledger of record, quota state, and billing hand-off to the
  Merchant-of-Record (section 7). Never in the request path of a hosted app.
- Data plane. AWS. Runs user SSR containers and serves them at
  `<app>.serve.pekoui.com` behind CloudFront. Emits raw usage signals; holds no
  platform identity.
- Metering plane. AWS-side collection (CloudWatch, access logs, build records)
  feeding an aggregation pipeline that produces per-app per-interval rollups,
  writes a billing-grade ledger, and mirrors display rollups to the control
  plane. This is the connective tissue that makes the data plane accountable.

The isolation rule from section 2 carries through: hosted sites live under
`serve.pekoui.com`, distinct from the `app.pekoui.com` control-plane parent, and
the host-only auth-cookie rule keeps a hosted site from reading platform
credentials.

## 2. AWS account and tenancy model

### 2.1 Account structure

> Recommendation: AWS Organizations with a dedicated hosting workload account,
> separate from any account holding platform or corporate resources. Use at least
> two workload accounts: `peko-hosting-prod` and `peko-hosting-staging`.

Rationale. A dedicated account gives a clean Cost and Usage Report (every line
in it is hosting cost), a hard blast-radius boundary for running other people's
code, and simple service-quota isolation. The staging account lets deploy and
metering changes be exercised end to end before they touch tenant traffic.

### 2.2 Tenancy

Tenancy is per app, not per account. One Peko account can own many hosted apps
(section 4.1), and each hosted app is one isolated runtime. Apps do not share a
container, a process, or a filesystem.

### 2.3 The tagging taxonomy (the backbone of tracking)

Every AWS resource the orchestration backend creates carries a fixed tag set.
These tags are activated as Cost Allocation Tags, so they appear in the Cost and
Usage Report, and they are the join key for every metering path.

- `peko:account_id` - the owning Peko account (Firebase uid or account id).
- `peko:app_id` - the platform app ID from section 4.1 (stable, not the
  `peko.toml` bundle id).
- `peko:deployment_id` - the specific deployment/build that produced the running
  version. Lets a metric spike be attributed to the deploy that caused it.
- `peko:plan_tier` - free or the paid tier, so enforcement and reporting can key
  off it without a control-plane lookup.
- `peko:env` - prod or staging.

Tag application is enforced at creation time by the orchestration backend and
audited by an AWS Config rule that flags any untagged billable resource. An
untagged resource is unattributable revenue leakage, so it is treated as an
incident, not a warning.

## 3. The deploy pipeline

This realizes the `peko deploy` handshake from section 18 as a durable,
observable state machine.

### 3.1 The orchestration backend

> Decision: the orchestration backend is a standalone service, not part of the
> SvelteKit app (section 8 requires this so a deploy workload cannot stall the
> web UI). It exposes an authenticated HTTP API that `peko deploy` calls and that
> `app.pekoui.com` calls on behalf of the dashboard.

It authenticates the same token `peko login` issues (section 4.2.1), verifies the
caller owns the target app ID, and is the only actor with credentials to mutate
AWS hosting resources. The CLI and the web app never touch AWS directly.

### 3.2 Deploy as a state machine

> Recommendation: model each deploy as an AWS Step Functions execution, one per
> deployment, tagged with the deployment id. Step Functions gives a durable,
> inspectable lifecycle, per-step retries, and a natural place to record status
> transitions the dashboard can show.

The stages:

1. Upload. `peko deploy` (authenticated, carrying the app ID) uploads the build
   artifact to a per-app prefix in S3. The backend records a new deployment row
   in the control plane in state `uploaded`.
2. Build. A build job turns the artifact plus framework adapter into a container
   image. Recommendation: AWS CodeBuild, for the same reason App Runner is the
   default runtime, it is the managed path with no build fleet to operate. Build
   minutes are a metered resource (section 5), so the build job emits a usage
   record keyed by app and deployment.
3. Push. The image is pushed to ECR. Recommendation: one ECR repository per app,
   named by app ID, with a lifecycle policy that keeps the last N images and
   expires older ones to cap storage cost.
4. Release. The backend creates or updates the app's App Runner service to the
   new image, with the tag set from section 2.3 and the resource caps from the
   app's plan tier (section 7). Deployment moves to `live`; the previous image is
   retained for rollback.
5. Route. The backend ensures the app's `<app>.serve.pekoui.com` record and TLS
   are in place (section 4).

Failure at any stage transitions the deployment to a terminal `failed` state
with the failing step and its logs referenced, which the dashboard surfaces.

## 4. Runtime

### 4.1 App Runner first, Fargate as escalation

> Decision (from section 18, restated): one App Runner service per app is the
> default. Fargate is a per-app escalation when a framework or resource profile
> does not fit App Runner. Lambda plus API Gateway stays rejected as the default
> because not every framework fits it cleanly.

Build against App Runner first. It provides managed containers, request-driven
autoscaling to a floor, and HTTPS without an ALB or task-definition surface to
own. The Fargate path is added only when a real app needs it, and reuses the same
tags, metering, and routing so the metering plane does not care which runtime a
given app is on.

### 4.2 Edge, routing, and TLS

Each app is reachable at `<app>.serve.pekoui.com`, fronted by CloudFront with a
certificate from ACM (section 18). CloudFront is not optional here: it is the
single choke point where per-request metering, WAF rate limiting, and egress
accounting all attach. ALB is introduced only alongside Fargate where App Runner
is not the origin.

### 4.3 Free-tier enforcement knobs

The App Runner service configuration is where hard caps live: max concurrency per
instance, max instance count, and instance size (vCPU and memory) are set from
the app's plan tier at release time. CloudFront plus WAF caps request rate and
body size. These hard limits protect the platform; the soft, metered limits in
section 7 handle overage billing above the free tier.

## 5. The metering plane

This is the core of the enterprise requirement. It has three signal sources, one
aggregation pipeline, and two grades of output.

### 5.1 Signal sources

- Compute. App Runner (and Fargate) publish per-service metrics to CloudWatch:
  active instance count, CPU and memory utilization, request count, latency,
  and 2xx/4xx/5xx counts. Instance-seconds by size give vCPU-seconds and
  GB-seconds per app. Recommendation: a CloudWatch metric stream to Kinesis Data
  Firehose so compute metrics flow into the same pipeline as everything else
  rather than being polled.
- Traffic. CloudFront real-time logs deliver per-request records (host, status,
  bytes downloaded and uploaded, edge latency) to a Kinesis stream. The host maps
  to app ID. This is the source of truth for request counts and egress bytes,
  the two metrics users watch most and a primary billed dimension.
- Build. CodeBuild emits build duration per job; the build step (section 3.2)
  writes a usage record keyed by app and deployment.

### 5.2 Aggregation pipeline

> Recommendation: Kinesis Data Firehose for ingestion, a stream aggregator
> (Lambda for launch simplicity, Kinesis Data Analytics / Managed Flink if
> volume outgrows it) that rolls raw events into per-app per-minute buckets, and
> two sinks.

Raw per-request and per-metric events are aggregated into per-app, per-minute
rollups: request count, error count, egress bytes, compute-seconds by dimension,
p50/p95/p99 latency. Per-minute rolls up to per-hour and per-day for retention at
decreasing resolution.

### 5.3 Two grades of output

- Live (fast, approximate). Per-minute rollups written to a time-series store.
  Recommendation: Amazon Timestream, for cheap time-partitioned retention with
  automatic tiering. These drive live dashboards and alerting. They may be
  seconds-to-minutes behind and are explicitly not billing authority.
- Billed (reconciled, authoritative). An append-only usage ledger keyed by
  (account_id, app_id, dimension, hour). Recommendation: DynamoDB for the ledger,
  reconciled daily against the AWS Cost and Usage Report delivered to S3 and
  queried with Athena, joined on the section 2.3 tags. When the reconciled CUR
  disagrees with the live sum, the ledger is corrected and the correction is
  recorded. The MoR bills from the ledger, never from the live store.

The reason for two grades: users need a number that updates now, and finance
needs a number that ties out to the AWS invoice. One store cannot be both. The
ledger is the single source of billing truth; the live store is a view.

### 5.4 Retention and resolution

- Per-minute: 48 hours (live dashboards, incident forensics).
- Per-hour: 90 days (trend views).
- Per-day: 13 months (year-over-year, invoices).

Retention tiers are a cost lever and are documented so the free tier's storage
footprint is bounded.

## 6. User-facing metrics, logs, and dashboards

Everything a user sees is served by the control plane reading rollups, never by
querying AWS directly. The orchestration backend mirrors display rollups from the
live store into Firestore on a short cadence; `app.pekoui.com` renders from
Firestore, so tenant-facing reads never hit tenant AWS resources and cannot leak
across apps.

### 6.1 What a user sees per app

- Live: requests per second, p50/p95/p99 latency, error rate, active instances.
- Historical: requests, egress, compute-seconds, and estimated cost over
  selectable ranges (hour, day, month, billing period).
- Per deployment: which deployment is live, deploy history, and the metric delta
  around each release so a regression is attributable to a deploy.
- Quota: current usage against the plan cap, with headroom, per dimension.

### 6.2 Logs

Container stdout and stderr flow to CloudWatch Logs in a per-app log group. The
control plane exposes searchable history and a live tail through the
orchestration backend (a scoped, short-lived read against only that app's log
group). Users never receive AWS credentials.

### 6.3 Programmatic access (enterprise)

A read-only metrics and usage API on the control plane, authenticated by the same
token model, returns the same rollups and ledger entries as JSON for customers
who want to pull usage into their own systems. Webhooks fire on quota thresholds
(for example, 80 percent and 100 percent of a free-tier cap).

### 6.4 Alerts and notifications

Threshold crossings (error-rate spike, latency SLO breach, quota approach) raise
a notification through the control plane. These derive from the same live
rollups, so what triggers an alert is the same number shown on the dashboard.

## 7. Billing and free-tier enforcement

- The free tier is defined by per-dimension caps: compute-seconds, requests,
  egress bytes, build minutes, and app count per account. The concrete numbers
  are the open item flagged in section 18 of the master reference and are set
  before launch.
- Enforcement is two-layer. Hard limits (section 4.3) cap what a single app can
  consume so it cannot run away. Soft limits are metered: usage above the free
  cap accrues in the ledger and is billed pay-as-you-go through the MoR.
- A quota loop in the control plane reads the live rollups, compares against caps,
  and can throttle or suspend an app that exceeds hard policy (tie-in to the AUP,
  section 7 of the master reference). Suspension is a control-plane action that
  scales the App Runner service to zero, not a data-plane decision.
- The MoR reads the reconciled ledger (section 5.3), so a customer's invoice and
  their dashboard cost estimate reconcile to the same source.

## 8. Isolation, security, and abuse

- Compute isolation is per app: one App Runner service (or Fargate task family)
  per app, no shared process or disk.
- Network isolation. Hosted apps run without access to platform-internal
  networks. Any per-app persistence a framework needs is provisioned per app and
  scoped by IAM to that app's resources only.
- Edge protection. WAF on CloudFront enforces rate limits, body-size limits, and
  common-exploit rules. This is also the first line against abuse of the free
  tier.
- Secrets. Per-app secrets live in AWS Secrets Manager or SSM Parameter Store,
  scoped to that app's execution role. The orchestration backend's role is the
  only identity that can create hosting resources, and it is least-privileged to
  exactly that.
- Auditability. CloudTrail in the hosting account records every control action;
  AWS Config enforces the tagging and isolation invariants.

## 9. Operator observability

Distinct from tenant-facing metrics, the operator needs a fleet view: aggregate
health across all apps, cost trend against revenue, deploy success rate, and
per-app anomaly detection. This runs off the same rollups plus CloudTrail and CUR
and lives on the admin surface (section 5.2 of the master reference).

## 10. Control-plane data model (additions)

New entities the control plane owns:

- App (extends the section 4.1 project): plan tier, current deployment id,
  serve hostname, status, AWS service reference.
- Deployment: app id, artifact reference, image reference, state
  (uploaded, building, live, failed, rolled-back), timestamps, build minutes.
- UsageRecord (ledger): account id, app id, dimension, hour bucket, quantity,
  reconciled flag, source (live or CUR).
- MetricRollup (display mirror): app id, interval, dimension values; short TTL.
- QuotaState: account id, per-dimension usage in the current period, cap, tier.

## 11. Milestones

Built thinnest vertical slice first, matching section 18's "its own milestones."

1. One-app happy path. Orchestration backend, `peko deploy` upload, CodeBuild to
   ECR, App Runner release, `<app>.serve.pekoui.com` with ACM and CloudFront, for
   one target SSR framework. No metering yet. Proves the deploy contract.
2. Metering plane, live grade. CloudFront real-time logs and CloudWatch metric
   stream into Firehose, per-minute rollups into Timestream, mirrored to
   Firestore. Dashboard shows live requests, latency, errors, egress.
3. Ledger and reconciliation. DynamoDB usage ledger, CUR-to-S3 plus Athena daily
   reconciliation, cost estimate on the dashboard.
4. Quotas and billing. Free-tier caps enforced (hard at App Runner and WAF, soft
   via ledger), quota loop, MoR hand-off, threshold notifications.
5. Enterprise surfaces. Logs search and live tail, metrics API, webhooks,
   per-deployment attribution, operator fleet view.
6. Fargate escalation path and any framework that App Runner cannot host.

## 12. Open items (need a decision before or during build)

1. First target SSR framework for milestone 1 (drives the build adapter). The
   master reference names Next-SSR, Django, and Flask as examples.
2. Build service: managed CodeBuild (recommended) versus a self-run builder on
   Fargate, a cost-versus-control call.
3. Concrete free-tier caps per dimension (the open item already flagged in
   section 18 of the master reference).
4. Whether an existing AWS Organizations setup is reused or a fresh org is stood
   up for the hosting workload accounts.
5. Time-series store choice if Timestream's cost or query model does not fit at
   volume (fallback: a rollup table in the same DynamoDB or a Postgres/Timescale
   instance).
