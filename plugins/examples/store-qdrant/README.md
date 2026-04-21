# store-qdrant

Reference `storage.vector` sidecar that proxies to a local or remote
[Qdrant](https://qdrant.tech/) HTTP API. Stdlib-only Python — no
`qdrant-client` dependency.

## Setup

```bash
# 1. Run Qdrant (Docker is the fastest path):
docker run -p 6333:6333 -v $(pwd)/qdrant_storage:/qdrant/storage \
  qdrant/qdrant

# 2. Point the sidecar at it.
export QDRANT_URL=http://127.0.0.1:6333
export QDRANT_COLLECTION=smiths-calls
export QDRANT_VECTOR_SIZE=384            # match your embedding model
export QDRANT_DISTANCE=Cosine            # Cosine | Dot | Euclid | Manhattan
# Optional:
export QDRANT_API_KEY=...
```

Drop this directory under `plugins.dir`. On first `upsert` / `search`
the sidecar auto-creates the collection with the configured size +
distance.

## Engine wiring

Slice 3.4 adds the `storage.vector` capability namespace but the
engine's adapter that plugs this sidecar in behind `VectorStore`
trait-objects is a follow-on. For now the sidecar advertises
`describe_capabilities` and serves `upsert` / `search` / `delete` /
`count` over JSON-RPC; operators can invoke it via the raw
`AiProvider::invoke` path until the native adapter lands.

## I/O shape

Contract matches the in-tree `VectorStore` trait:

```json
// search request
{"jsonrpc":"2.0","method":"search","id":1,
 "params":{"query":[0.1,0.2,...],"k":5}}

// response
{"jsonrpc":"2.0","id":1,"result":{
  "hits":[{"id":"call-42","score":0.87,"metadata":{...}}]
}}
```

`id` is always the caller's original string key; the sidecar hashes
it to a 63-bit integer for Qdrant's point id + stashes the original
in `payload._smiths_id` so round-trips are lossless.

## Priority

Advertises `priority = 20` — no other in-tree vector backend to fail
over to today, but the field is there for future ones (pgvector,
Weaviate, Pinecone).
