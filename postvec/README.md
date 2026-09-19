# postvec (extension)

Postvec is an extension for fused hybrid search: it combines BM25 and semantic search in one SQL call and keeps vectors in sync with your text automatically. You can also use it to migrate vectors without re-embedding of text with direct conversion between embedding formats.

Install and usage: the [root README](../README.md) and [postvec.dev/docs](https://postvec.dev/docs/).

PostgreSQL 16 / 17 / 18, pgvector >= 0.8, `shared_preload_libraries = 'postvec'`.

```sql
CREATE EXTENSION postvec CASCADE;
SELECT postvec.enable('public.docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
```

`postvec.mode` defaults to `embedded` (engine in the launcher). `grpc` talks to [postvec-server](../postvec-server). Published packages include both. A `cargo` build without `--features embedded` is the gRPC client only.

Configure a cluster with the [`postvec` CLI](../postvec-cli):

```bash
sudo postvec setup --database app --embedded
postvec doctor --database app
```

SQL reference: [postvec.dev/docs/reference/sql](https://postvec.dev/docs/reference/sql). Tests: `cd postvec && ./ci.sh`.
