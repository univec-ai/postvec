---
layout: page
sidebar: false
title: postvec
titleTemplate: postvec
description: A PostgreSQL extension for in-database embeddings, hybrid search, and in-place vector migration.
---

<HomePage>
  <template #example>

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2',
  create_fts_index => true
);

SELECT d.id, d.body, s.rrf_score
  FROM postvec.search(
         'public.docs', 'body',
         'changing an embedding model',
         filter => '{"category":"engineering"}'::jsonb
       ) AS s
  JOIN public.docs AS d
    ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

  </template>
</HomePage>
