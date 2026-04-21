# store-s3-recording

Reference `storage.recording` sidecar that stores per-call audio in
any S3-compatible object store. Shells out to the `aws` CLI so
SigV4 signing + credential discovery (env vars, IAM role, SSO
profiles) stays in one well-tested place.

## Setup

```bash
# 1. Install the aws CLI:
brew install awscli
# or: pip install awscli

# 2. Configure credentials (any method the CLI accepts works):
aws configure          # interactive — asks for key + secret + region
# or: export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... AWS_REGION=...

# 3. Point the sidecar at a bucket.
export S3_BUCKET=smiths-recordings-prod
export S3_PREFIX=recordings            # default
# For MinIO / R2 / B2:
# export S3_ENDPOINT_URL=https://<host>
# export S3_REGION=auto
```

Drop this directory under `plugins.dir`. The sidecar advertises
`storage.recording` and serves `put` / `get` / `delete` / `list` /
`prune_older_than` over JSON-RPC.

## Engine wiring

Slice 3.4 adds the `storage.recording` capability namespace and the
native FS backend; the adapter that plugs this sidecar in behind
the `RecordingStore` trait is a follow-on (the engine already knows
how to dispatch JSON-RPC calls to loaded sidecars — only the
trait-adapter shim is missing).

## Keys

Each call lands at `s3://$S3_BUCKET/$S3_PREFIX/<call_id>.wav` with
`/` in call ids substituted as `_` to keep object keys flat. The
original id round-trips through `list` by reversing that.

## Retention

Slice 3.4 defines `[storage.recording] retention_days` on the
engine config. The native FS backend prunes via on-disk mtime; the
S3 sidecar exposes the equivalent as `prune_older_than(max_age_secs)`
— ready for the engine adapter to call on its hourly sweep.
