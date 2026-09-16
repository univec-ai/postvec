---
layout: page
sidebar: false
title: postvec
titleTemplate: postvec
description: Full-text and semantic search in one call. Embedding and vector conversion inside the database with local models or via external providers.
---

<HomePage>
  <template #enable>

```sql
CREATE TABLE docs (
  id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  body     text,
  category text
);

SELECT postvec.enable(
  'public.docs', 'body',
  model            => 'sentence-transformers-all-minilm-l6-v2',
  create_fts_index => true
);
```

  </template>
  <template #search>

```sql
SELECT d.id, d.body, s.rrf_score
  FROM postvec.search(
         'public.docs', 'body',
         'embedding model migration',
         filter => '{"category": "engineering"}'::jsonb
       ) AS s
  JOIN public.docs AS d
    ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

  </template>
  <template #adopt>

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model         => 'openai-text-embedding-ada-002',
  backfill      => 'none'
);
```

  </template>
  <template #migrate>

```sql
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3'
) AS migration_id \gset

SELECT state, rows_done, rows_total, progress_pct
  FROM postvec.migration_status(:migration_id);

-- After state reaches 'awaiting_finalize':
SELECT postvec.migration_finalize(:migration_id);
```

  </template>
</HomePage>
