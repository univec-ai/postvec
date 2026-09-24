-- SPDX-License-Identifier: PostgreSQL
CREATE OR REPLACE FUNCTION postvec._owner(relation regclass) RETURNS void LANGUAGE plpgsql
SET search_path=pg_catalog AS $$
BEGIN
    PERFORM pg_advisory_xact_lock_shared(1886615158,1);
    IF NOT EXISTS(SELECT FROM pg_class WHERE oid=relation AND relkind IN ('r','p') AND pg_has_role(current_user,relowner,'USAGE')) THEN
        RAISE EXCEPTION 'table ownership is required';
    END IF;
    PERFORM pg_advisory_xact_lock(1886615158,3);
    EXECUTE format('LOCK TABLE %s IN ACCESS EXCLUSIVE MODE',relation);
END $$;

CREATE OR REPLACE FUNCTION postvec._refs(template text) RETURNS text[] LANGUAGE plpgsql IMMUTABLE
SET search_path=pg_catalog AS $$
DECLARE i integer:=1; c text; name text; closed boolean; refs text[]:='{}';
BEGIN
    IF template IS NULL THEN RETURN refs; END IF;
    IF octet_length(template) NOT BETWEEN 1 AND 16384 THEN RAISE EXCEPTION 'invalid format length'; END IF;
    WHILE i<=length(template) LOOP
        c:=substr(template,i,1); i:=i+1;
        IF c<>'$' THEN CONTINUE; END IF;
        c:=substr(template,i,1); name:='';
        IF c='$' THEN i:=i+1; CONTINUE;
        ELSIF c='{' THEN
            i:=i+1; closed:=false;
            WHILE i<=length(template) LOOP
                c:=substr(template,i,1); i:=i+1;
                IF c='}' THEN
                    IF substr(template,i,1)='}' THEN i:=i+1;
                    ELSE closed:=true; EXIT; END IF;
                END IF;
                name:=name||c;
            END LOOP;
            IF NOT closed THEN RAISE EXCEPTION 'unclosed format reference'; END IF;
        ELSE
            IF c!~'^[A-Za-z_]$' THEN RAISE EXCEPTION 'invalid format reference'; END IF;
            WHILE substr(template,i,1)~'^[A-Za-z0-9_]$' LOOP
                name:=name||substr(template,i,1); i:=i+1;
            END LOOP;
        END IF;
        IF name='' THEN RAISE EXCEPTION 'empty format reference'; END IF;
        IF NOT name=ANY(refs) THEN refs:=refs||name; END IF;
    END LOOP;
    RETURN refs;
END $$;

CREATE OR REPLACE FUNCTION postvec._format(relation regclass, template text, source_column text, chunked boolean)
RETURNS void LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE refs text[]; name text;
BEGIN
    IF template IS NULL THEN RETURN; END IF;
    refs:=postvec._refs(template);
    FOREACH name IN ARRAY refs LOOP
        IF chunked AND name='chunk' THEN CONTINUE; END IF;
        IF NOT EXISTS(SELECT FROM pg_attribute WHERE attrelid=relation AND attname=name AND attnum>0 AND NOT attisdropped AND atttypid IN ('text'::regtype,'varchar'::regtype,'bpchar'::regtype,'int2'::regtype,'int4'::regtype,'int8'::regtype,'float4'::regtype,'float8'::regtype,'numeric'::regtype,'bool'::regtype,'uuid'::regtype,'date'::regtype,'timestamp'::regtype,'timestamptz'::regtype)) THEN RAISE EXCEPTION 'unsupported format column %',name; END IF;
    END LOOP;
    IF NOT (CASE WHEN chunked THEN 'chunk' ELSE source_column END)=ANY(refs) THEN RAISE EXCEPTION 'format must reference %',CASE WHEN chunked THEN '$chunk' ELSE source_column END; END IF;
END $$;

CREATE OR REPLACE FUNCTION postvec._enqueue() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,pg_temp
SET DateStyle TO 'ISO, MDY' SET TimeZone TO 'UTC' SET IntervalStyle TO 'postgres' AS $$
DECLARE r postvec.registry; refs text[]; pk text; oldpk text; expr text; pred text; changed boolean; item record;
    nkey text; okey text; jpred text; job_op text;
BEGIN
    SELECT * INTO r FROM postvec.registry WHERE id=TG_ARGV[0]::bigint AND state<>'disabled'
      AND (to_regclass(format('%I.%I',table_schema,table_name))=TG_RELID OR TG_RELID IN (SELECT relid FROM pg_partition_tree(to_regclass(format('%I.%I',table_schema,table_name)))));
    IF NOT FOUND THEN RETURN NULL; END IF;
    job_op := CASE WHEN r.chunking='recursive' THEN 'refresh' ELSE 'embed' END;
    IF TG_OP='UPDATE' THEN
        refs := postvec._refs(r.format);
        IF r.chunking='recursive' THEN refs := array_remove(refs,'chunk'); END IF;
        IF NOT r.source_column=ANY(refs) THEN refs := r.source_column||refs; END IF;
    END IF;
    IF TG_LEVEL='STATEMENT' THEN
        SELECT string_agg(format('n.%I',c),','), string_agg(format('o.%I',c),','), string_agg(format('n.%I=o.%I',c,c),' AND ')
          INTO nkey, okey, jpred FROM unnest(r.pk_columns) c;
        IF cardinality(r.pk_columns)>1 THEN nkey:='ROW('||nkey||')::text'; okey:='ROW('||okey||')::text';
        ELSE nkey:=nkey||'::text'; okey:=okey||'::text'; END IF;
        IF TG_OP='INSERT' THEN
            EXECUTE format('INSERT INTO postvec.jobs(registry_id,pk_value,op) SELECT $1,%s,%L FROM new_table n WHERE n.%I IS NOT NULL ON CONFLICT(registry_id,op,pk_value,chunk_id) WHERE claimed_at IS NULL DO NOTHING', nkey, job_op, r.source_column) USING r.id;
        ELSIF TG_OP='DELETE' THEN
            EXECUTE format('DELETE FROM postvec.jobs_dead d USING old_table o WHERE d.registry_id=$1 AND d.pk_value=%s', okey) USING r.id;
            EXECUTE format('DELETE FROM postvec.jobs j USING old_table o WHERE j.registry_id=$1 AND j.pk_value=%s AND j.claimed_at IS NULL', okey) USING r.id;
            IF r.chunking='recursive' THEN
                EXECUTE format('DELETE FROM %I.%I c USING old_table o WHERE c.postvec_source_pk=o.%I', r.destination_schema, r.destination_table, r.pk_columns[1]);
            END IF;
        ELSIF TG_OP='UPDATE' THEN
            SELECT string_agg(format('n.%1$I IS DISTINCT FROM o.%1$I',c),' OR ') INTO pred FROM unnest(refs) c;
            EXECUTE format(
                'WITH changed AS (SELECT %s AS pk FROM new_table n JOIN old_table o ON %s WHERE %s)%s
                 INSERT INTO postvec.jobs(registry_id,pk_value,op) SELECT $1,pk,%L FROM changed
                 ON CONFLICT(registry_id,op,pk_value,chunk_id) WHERE claimed_at IS NULL DO NOTHING',
                nkey, jpred, pred,
                CASE WHEN r.chunking='recursive' THEN format(', dc AS (DELETE FROM %I.%I c USING changed x WHERE c.postvec_source_pk=x.pk::%s), dj AS (DELETE FROM postvec.jobs j USING changed x WHERE j.registry_id=$1 AND j.op=''embed'' AND j.pk_value=x.pk), dd AS (DELETE FROM postvec.jobs_dead d USING changed x WHERE d.registry_id=$1 AND d.pk_value=x.pk)', r.destination_schema, r.destination_table, r.pk_types[1])
                     ELSE ', dd AS (DELETE FROM postvec.jobs_dead d USING changed x WHERE d.registry_id=$1 AND d.pk_value=x.pk)' END,
                job_op) USING r.id;
        END IF;
        PERFORM postvec.worker_kick(); RETURN NULL;
    END IF;
    SELECT string_agg(format('($1).%I',c),',') INTO expr FROM unnest(r.pk_columns) c;
    IF cardinality(r.pk_columns)>1 THEN expr:='ROW('||expr||')'; END IF;
    IF TG_OP<>'INSERT' THEN EXECUTE 'SELECT ('||expr||')::text' INTO oldpk USING OLD; END IF;
    IF TG_OP<>'DELETE' THEN EXECUTE 'SELECT ('||expr||')::text' INTO pk USING NEW; END IF;
    IF TG_OP='UPDATE' AND oldpk=pk THEN
        SELECT string_agg(format('($1).%1$I IS DISTINCT FROM ($2).%1$I',c),' OR ') INTO pred FROM unnest(refs) c;
        EXECUTE 'SELECT '||pred INTO changed USING NEW, OLD;
        IF NOT changed THEN RETURN NULL; END IF;
    ELSIF TG_OP='INSERT' THEN
        EXECUTE format('SELECT ($1).%I IS NULL',r.source_column) INTO changed USING NEW;
        IF changed THEN RETURN NULL; END IF;
    END IF;
    FOR item IN SELECT DISTINCT x FROM unnest(ARRAY[oldpk,pk]) x WHERE x IS NOT NULL LOOP
        IF r.chunking='recursive' THEN
            EXECUTE format('DELETE FROM %I.%I WHERE postvec_source_pk=$1::%s',r.destination_schema,r.destination_table,r.pk_types[1]) USING item.x;
            DELETE FROM postvec.jobs WHERE registry_id=r.id AND pk_value=item.x AND op='embed';
        END IF;
        DELETE FROM postvec.jobs_dead WHERE registry_id=r.id AND pk_value=item.x;
    END LOOP;
    IF oldpk IS NOT NULL AND oldpk IS DISTINCT FROM pk THEN
        DELETE FROM postvec.jobs WHERE registry_id=r.id AND pk_value=oldpk AND claimed_at IS NULL;
    END IF;
    IF pk IS NOT NULL THEN
        INSERT INTO postvec.jobs(registry_id,pk_value,op) VALUES(r.id,pk,job_op)
        ON CONFLICT(registry_id,op,pk_value,chunk_id) WHERE claimed_at IS NULL DO NOTHING;
    END IF;
    PERFORM postvec.worker_kick(); RETURN NULL;
END $$;

DROP FUNCTION IF EXISTS postvec._existing(regclass,text,text,text);
CREATE OR REPLACE FUNCTION postvec._existing(relation regclass, col text, want jsonb) RETURNS bigint
LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE r postvec.registry; drift text;
BEGIN
    SELECT * INTO r FROM postvec.registry WHERE to_regclass(format('%I.%I',table_schema,table_name))=relation AND source_column=col AND state<>'disabled';
    IF NOT FOUND THEN RETURN NULL; END IF;
    SELECT string_agg(format('%s %s (requested %s)',k,to_jsonb(r)->k,v),', ' ORDER BY k) INTO drift FROM jsonb_each(want) e(k,v) WHERE to_jsonb(r)->k IS DISTINCT FROM v;
    IF drift IS NOT NULL THEN
        RAISE EXCEPTION '%.% is already enabled with %; disable() it first to change them',relation,col,drift;
    END IF;
    RETURN r.id;
END $$;

DROP FUNCTION IF EXISTS postvec._register(regclass, text, text, text, boolean, boolean, text, text, text, boolean, text, text, text, integer, integer, text);
CREATE OR REPLACE FUNCTION postvec._register(relation regclass, col text, model_name text, vec text, adopted boolean, trig_mode text, backfill text, distance text, fts text, fts_index boolean, template text, index_mode text, chunking text, chunk_size integer, chunk_overlap integer, destination text)
RETURNS bigint LANGUAGE plpgsql SET search_path=pg_catalog SET DateStyle TO 'ISO, MDY' SET TimeZone TO 'UTC' SET IntervalStyle TO 'postgres' AS $$
DECLARE r postvec.registry; ns text; tbl text; vt text; dimension integer; keys text[]; types text[]; expr text; owner_oid oid; pkdef text; dest text[]; pkwhen text;
BEGIN
    PERFORM postvec._owner(relation);
    IF distance NOT IN ('cosine','l2','ip') OR index_mode NOT IN ('manual','auto','immediate') OR backfill NOT IN ('none','queue','cursor') OR chunking NOT IN ('none','recursive') OR trig_mode NOT IN ('statement','row','none') THEN RAISE EXCEPTION 'invalid registry option'; END IF;
    IF NOT EXISTS(SELECT FROM pg_attribute WHERE attrelid=relation AND attname=col AND attnum>0 AND NOT attisdropped AND atttypid IN ('text'::regtype,'varchar'::regtype,'bpchar'::regtype)) THEN RAISE EXCEPTION 'source must be a text column'; END IF;
    SELECT n.nspname,c.relname,c.relowner INTO ns,tbl,owner_oid FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.oid=relation;
    IF EXISTS(SELECT FROM postvec.registry WHERE table_schema=ns AND table_name=tbl AND source_column=col) THEN RAISE EXCEPTION '%.%.% is already enabled (disable() it first)',ns,tbl,col; END IF;
    IF trig_mode='statement' AND EXISTS(SELECT FROM pg_class WHERE oid=relation AND relkind='p') THEN
        RAISE WARNING 'statement triggers cover writes through the parent only; use trigger_mode => row for direct partition writes';
    END IF;
    IF ns='postvec' OR ns LIKE 'pg_%' THEN RAISE EXCEPTION 'source must be a user table'; END IF;
    SELECT array_agg(a.attname::text ORDER BY k.ord),array_agg(format_type(a.atttypid,a.atttypmod) ORDER BY k.ord)
      INTO keys,types FROM pg_index i CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY k(attnum,ord)
      JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attnum=k.attnum WHERE i.indrelid=relation AND i.indisprimary AND k.ord<=i.indnkeyatts;
    IF keys IS NULL THEN RAISE EXCEPTION 'primary key required'; END IF;
    SELECT format('%I.vector',n.nspname) INTO vt FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='vector';
    dimension:=COALESCE((SELECT dim FROM postvec._route(model_name) WHERE dim>0),(SELECT target_dim FROM postvec.models WHERE model_type='convert' AND target_model=model_name AND target_dim>0 ORDER BY name LIMIT 1));
    IF adopted THEN
        SELECT atttypmod INTO STRICT dimension FROM pg_attribute WHERE attrelid=relation AND attname=vec AND atttypid=vt::regtype AND NOT attisdropped AND attnum>0;
        IF EXISTS(SELECT FROM pg_attribute WHERE attrelid=relation AND attname=vec AND (attgenerated<>'' OR attidentity<>'' OR atthasdef OR (attnotnull AND (trig_mode<>'none' OR backfill<>'none')))) THEN RAISE EXCEPTION 'vector column has incompatible generation, default or NOT NULL'; END IF;
        IF EXISTS(SELECT FROM postvec.models WHERE (target_model=model_name OR name=model_name) AND target_dim IS NOT NULL AND target_dim<>dimension) THEN RAISE EXCEPTION 'model dimension contradicts vector column'; END IF;
    END IF;
    IF dimension IS NULL OR dimension NOT BETWEEN 1 AND 16000 THEN RAISE EXCEPTION 'model dimension unavailable; wait for model refresh'; END IF;
    IF octet_length(vec)>63 OR vec=col OR vec=ANY(keys) THEN RAISE EXCEPTION 'invalid vector column'; END IF;
    PERFORM postvec._format(relation,template,col,chunking='recursive');
    IF chunking='recursive' AND (adopted OR cardinality(keys)<>1 OR col='chunk' OR chunk_size NOT BETWEEN 64 AND 100000 OR chunk_overlap<0 OR chunk_overlap>=chunk_size) THEN RAISE EXCEPTION 'invalid recursive configuration'; END IF;
    INSERT INTO postvec.registry(table_schema,table_name,source_column,vector_column,pk_columns,pk_types,model,space,dim,fts_config,distance,trigger_mode,backfill_mode,owns_vector_column,format,index_mode,chunking,chunk_size,chunk_overlap,destination_schema,destination_table,destination_view,destination_token)
    VALUES(ns,tbl,col,vec,keys,types,model_name,(SELECT space FROM postvec._route(model_name)),dimension,fts::regconfig,distance,trig_mode,backfill,NOT adopted,template,index_mode,chunking,CASE WHEN chunking='recursive' THEN chunk_size END,CASE WHEN chunking='recursive' THEN chunk_overlap END,CASE WHEN chunking='recursive' THEN ns END,CASE WHEN chunking='recursive' THEN 'pending' END,CASE WHEN chunking='recursive' THEN 'pending' END,CASE WHEN chunking='recursive' THEN gen_random_uuid()::text END) RETURNING * INTO r;
    IF chunking='recursive' THEN
        dest:=parse_ident(destination);
        IF cardinality(dest) NOT IN (1,2) THEN RAISE EXCEPTION 'invalid destination'; END IF;
        r.destination_schema:=CASE WHEN cardinality(dest)=2 THEN dest[1] ELSE ns END;
        r.destination_table:=dest[cardinality(dest)]; r.destination_view:=r.destination_table||'_view';
        IF octet_length(r.destination_view)>63 THEN RAISE EXCEPTION 'destination name too long'; END IF;
        UPDATE postvec.registry SET destination_schema=r.destination_schema,destination_table=r.destination_table,destination_view=r.destination_view WHERE id=r.id;
        SELECT format_type(atttypid,atttypmod)||CASE WHEN attcollation<>0 THEN ' COLLATE '||attcollation::regcollation::text ELSE '' END INTO pkdef FROM pg_attribute WHERE attrelid=relation AND attname=keys[1];
        EXECUTE format('CREATE TABLE %I.%I(postvec_chunk_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,postvec_source_pk %s NOT NULL,postvec_chunk_seq integer NOT NULL,postvec_char_start bigint NOT NULL,postvec_char_end bigint NOT NULL,chunk_text text NOT NULL,%I %s(%s),UNIQUE(postvec_source_pk,postvec_chunk_seq))',r.destination_schema,r.destination_table,pkdef,vec,vt,dimension);
        EXECUTE format('COMMENT ON TABLE %I.%I IS %L',r.destination_schema,r.destination_table,format('postvec: managed chunk destination for %s.%s.%s (registry entry %s); ownership token %s. Automatic teardown requires this exact comment.',ns,tbl,col,r.id,r.destination_token));
        EXECUTE format('CREATE VIEW %I.%I WITH(security_invoker=true,security_barrier=true) AS SELECT c.postvec_source_pk AS pk_value,c.postvec_chunk_seq AS chunk_seq,c.postvec_char_start AS chunk_start,c.postvec_char_end AS chunk_end,c.chunk_text FROM %I.%I c JOIN %s s ON s.%I=c.postvec_source_pk',r.destination_schema,r.destination_view,r.destination_schema,r.destination_table,relation,keys[1]);
        EXECUTE format('COMMENT ON VIEW %I.%I IS %L',r.destination_schema,r.destination_view,format('postvec: managed chunk join view for %s.%s.%s (registry entry %s); ownership token %s. Automatic teardown requires this exact comment.',ns,tbl,col,r.id,r.destination_token));
        EXECUTE format('ALTER TABLE %I.%I ENABLE ROW LEVEL SECURITY; CREATE POLICY postvec_source_visible ON %I.%I FOR SELECT USING(EXISTS(SELECT FROM %s s WHERE s.%I=postvec_source_pk))',r.destination_schema,r.destination_table,r.destination_schema,r.destination_table,relation,keys[1]);
    ELSIF NOT adopted THEN EXECUTE format('ALTER TABLE %s ADD COLUMN %I %s(%s)',relation,vec,vt,dimension);
    END IF;
    EXECUTE format('CREATE TRIGGER %I AFTER TRUNCATE ON %s FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_truncate(%L)','postvec_trunc_'||r.id,relation,r.id);
    IF trig_mode='row' THEN
        EXECUTE format('CREATE TRIGGER %I AFTER INSERT OR UPDATE OR DELETE ON %s FOR EACH ROW EXECUTE FUNCTION postvec._enqueue(%L)','postvec_sync_'||r.id,relation,r.id);
    ELSIF trig_mode='statement' THEN
        SELECT string_agg(format('OLD.%I IS DISTINCT FROM NEW.%I',c,c),' OR ')
          INTO pkwhen FROM unnest(r.pk_columns) c;
        EXECUTE format('CREATE TRIGGER %I AFTER INSERT ON %s REFERENCING NEW TABLE AS new_table FOR EACH STATEMENT EXECUTE FUNCTION postvec._enqueue(%L)','postvec_ins_'||r.id,relation,r.id);
        EXECUTE format('CREATE TRIGGER %I AFTER UPDATE ON %s REFERENCING OLD TABLE AS old_table NEW TABLE AS new_table FOR EACH STATEMENT EXECUTE FUNCTION postvec._enqueue(%L)','postvec_upd_'||r.id,relation,r.id);
        EXECUTE format('CREATE TRIGGER %I AFTER DELETE ON %s REFERENCING OLD TABLE AS old_table FOR EACH STATEMENT EXECUTE FUNCTION postvec._enqueue(%L)','postvec_del_'||r.id,relation,r.id);
        EXECUTE format('CREATE TRIGGER %I AFTER UPDATE ON %s FOR EACH ROW WHEN (%s) EXECUTE FUNCTION postvec._enqueue(%L)','postvec_pk_'||r.id,relation,pkwhen,r.id);
    END IF;
    IF backfill='queue' THEN
        SELECT string_agg(format('%I',c),',') INTO expr FROM unnest(keys) c;
        IF cardinality(keys)>1 THEN expr:='ROW('||expr||')'; END IF;
        EXECUTE format('INSERT INTO postvec.jobs(registry_id,pk_value,op) SELECT $1,(%s)::text,$2 FROM %s WHERE %I IS NOT NULL',expr,relation,col) USING r.id,CASE WHEN chunking='recursive' THEN 'refresh' ELSE 'embed' END;
    END IF;
    IF fts_index THEN EXECUTE format('CREATE INDEX %I ON %s USING gin(to_tsvector(%L::regconfig,coalesce(%I::text,'''')))','postvec_fts_'||r.id,CASE WHEN chunking='recursive' THEN format('%I.%I',r.destination_schema,r.destination_table) ELSE relation::text END,fts,CASE WHEN chunking='recursive' THEN 'chunk_text' ELSE col END); END IF;
    IF index_mode='immediate' THEN PERFORM postvec.create_vector_index(relation,col); END IF;
    RAISE NOTICE 'Source text is sent to the configured postvec-server inference fleet';
    PERFORM postvec.worker_kick();RETURN r.id;
END $$;

DROP FUNCTION IF EXISTS postvec.enable(regclass,text,text,text,text,boolean,boolean,text,text,text,text,text,text,integer,integer,text);
CREATE OR REPLACE FUNCTION postvec.enable(relation regclass,column_name text,model text,vector_column text DEFAULT NULL,fts_config text DEFAULT 'pg_catalog.english',create_fts_index boolean DEFAULT false,backfill boolean DEFAULT true,distance text DEFAULT 'cosine',trigger_mode text DEFAULT 'statement',index_mode text DEFAULT 'manual',backfill_mode text DEFAULT 'queue',format text DEFAULT NULL,chunking text DEFAULT 'none',chunk_size integer DEFAULT NULL,chunk_overlap integer DEFAULT NULL,destination text DEFAULT NULL,if_not_exists boolean DEFAULT false)
RETURNS bigint LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE rid bigint; ns text; tbl text; dest text[];
BEGIN
    IF trigger_mode NOT IN ('statement','row') THEN RAISE EXCEPTION 'invalid trigger mode'; END IF;
    IF chunking='recursive' THEN
        SELECT n.nspname,c.relname INTO ns,tbl FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.oid=relation;
        destination:=coalesce(destination,quote_ident(tbl||'_'||column_name||'_chunks'));
        dest:=parse_ident(destination);
    END IF;
    IF if_not_exists THEN
        rid:=postvec._existing(relation::regclass,column_name,jsonb_build_object('model',model,'vector_column',coalesce(vector_column,column_name||'_semantic'),'distance',distance,'trigger_mode',trigger_mode,'index_mode',index_mode,'fts_config',fts_config::regconfig,'format',format,'chunking',chunking,
            'chunk_size',CASE WHEN chunking='recursive' THEN coalesce(chunk_size,1000) END,'chunk_overlap',CASE WHEN chunking='recursive' THEN coalesce(chunk_overlap,200) END,
            'destination_schema',CASE WHEN cardinality(dest)=2 THEN dest[1] ELSE ns END,'destination_table',dest[cardinality(dest)]));
        IF rid IS NOT NULL THEN RETURN rid; END IF;
    END IF;
    RETURN postvec._register(relation::regclass,column_name,model,coalesce(vector_column,column_name||'_semantic'),false,trigger_mode,CASE WHEN backfill THEN backfill_mode ELSE 'none' END,distance,fts_config,create_fts_index,format,index_mode,chunking,coalesce(chunk_size,1000),coalesce(chunk_overlap,200),destination);
END $$;
DROP FUNCTION IF EXISTS postvec.adopt(regclass,text,text,text,boolean,text,text,text,text,text,boolean,text,text);
CREATE OR REPLACE FUNCTION postvec.adopt(relation regclass,column_name text,vector_column text,model text,sync boolean DEFAULT true,backfill text DEFAULT 'missing',backfill_mode text DEFAULT 'queue',distance text DEFAULT 'cosine',trigger_mode text DEFAULT 'statement',fts_config text DEFAULT 'pg_catalog.english',create_fts_index boolean DEFAULT false,format text DEFAULT NULL,index_mode text DEFAULT 'manual',if_not_exists boolean DEFAULT false)
RETURNS bigint LANGUAGE plpgsql SET search_path=pg_catalog SET DateStyle TO 'ISO, MDY' SET TimeZone TO 'UTC' SET IntervalStyle TO 'postgres' AS $$
DECLARE rid bigint;
BEGIN
    IF backfill NOT IN ('missing','all','none') OR trigger_mode NOT IN ('statement','row') THEN RAISE EXCEPTION 'invalid adoption option'; END IF;
    IF if_not_exists THEN
        rid:=postvec._existing(relation::regclass,column_name,jsonb_build_object('model',model,'vector_column',vector_column,'distance',distance,'trigger_mode',CASE WHEN sync THEN trigger_mode ELSE 'none' END,'index_mode',index_mode,'fts_config',fts_config::regconfig,'format',format));
        IF rid IS NOT NULL THEN RETURN rid; END IF;
    END IF;
    rid:=postvec._register(relation::regclass,column_name,model,vector_column,true,CASE WHEN sync THEN trigger_mode ELSE 'none' END,CASE WHEN backfill='none' THEN 'none' ELSE backfill_mode END,distance,fts_config,create_fts_index,format,index_mode,'none',NULL,NULL,NULL);
    IF backfill='all' AND backfill_mode='cursor' THEN
        INSERT INTO postvec.settings(key,value) VALUES('backfill_all:'||rid,'true');
    END IF;
    IF backfill='missing' AND backfill_mode='queue' THEN
        EXECUTE format('DELETE FROM postvec.jobs j WHERE registry_id=$1 AND EXISTS(SELECT FROM %s s WHERE %I IS NOT NULL AND %s=j.pk_value)',relation::regclass,vector_column,(SELECT CASE WHEN cardinality(pk_columns)>1 THEN 'ROW('||string_agg(quote_ident(c),',')||')' ELSE string_agg(quote_ident(c),',') END||'::text' FROM postvec.registry CROSS JOIN LATERAL unnest(pk_columns) c WHERE id=rid GROUP BY pk_columns)) USING rid;
    END IF;
    RETURN rid;
END $$;

CREATE OR REPLACE FUNCTION postvec.set_format(relation regclass,column_name text,format text) RETURNS void LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE r postvec.registry;
BEGIN
    PERFORM postvec._owner(relation::regclass);
    SELECT * INTO STRICT r FROM postvec.registry WHERE to_regclass(pg_catalog.format('%I.%I',table_schema,table_name))=relation::regclass AND source_column=column_name FOR UPDATE;
    IF r.state<>'active' THEN RAISE EXCEPTION 'entry must be active'; END IF;
    PERFORM postvec._format(relation::regclass,format,column_name,r.chunking='recursive');
    IF r.format IS NOT DISTINCT FROM format THEN RETURN; END IF;
    UPDATE postvec.registry SET format=set_format.format,backfill_mode='cursor',backfill_watermark=NULL WHERE id=r.id;
    IF r.chunking='none' THEN EXECUTE pg_catalog.format('UPDATE %s SET %I=NULL',relation::regclass,r.vector_column);
    ELSE EXECUTE pg_catalog.format('DELETE FROM %I.%I',r.destination_schema,r.destination_table); END IF;
    DELETE FROM postvec.jobs WHERE registry_id=r.id;DELETE FROM postvec.jobs_dead WHERE registry_id=r.id;
    PERFORM postvec.worker_kick();
END $$;

CREATE OR REPLACE FUNCTION postvec.create_vector_index(relation regclass,column_name text) RETURNS void LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE r postvec.registry; ns text; expr text; op text;
BEGIN
    PERFORM postvec._owner(relation::regclass);
    SELECT * INTO STRICT r FROM postvec.registry WHERE to_regclass(format('%I.%I',table_schema,table_name))=relation::regclass AND source_column=column_name FOR UPDATE;
    IF r.state<>'active' THEN RAISE EXCEPTION 'entry must be active'; END IF;
    SELECT quote_ident(n.nspname) INTO ns FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='vector';
    op:=CASE r.distance WHEN 'l2' THEN 'l2' WHEN 'ip' THEN 'ip' ELSE 'cosine' END;
    expr:=CASE WHEN r.dim>2000 THEN format('(%I::%s.halfvec(%s)) %s.halfvec_%s_ops',r.vector_column,ns,r.dim,ns,op) ELSE format('%I %s.vector_%s_ops',r.vector_column,ns,op) END;
    EXECUTE format('CREATE INDEX %I ON %I.%I USING hnsw(%s)','postvec_vec_'||r.id,coalesce(r.destination_schema,r.table_schema),coalesce(r.destination_table,r.table_name),expr);
    UPDATE postvec.registry SET index_error=NULL WHERE id=r.id;
END $$;

CREATE OR REPLACE FUNCTION postvec.migrate(relation regclass,column_name text,new_model text,strategy text DEFAULT 'convert',reindex text DEFAULT 'manual',observed_writes_quiesced boolean DEFAULT false)
RETURNS bigint LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE r postvec.registry; converter text; dimension integer; vt text; rid bigint; total bigint; newcol text; via jsonb;
BEGIN
    PERFORM postvec._owner(relation::regclass);
    SELECT * INTO STRICT r FROM postvec.registry WHERE to_regclass(format('%I.%I',table_schema,table_name))=relation::regclass AND source_column=column_name FOR UPDATE;
    IF r.state<>'active' OR r.model=new_model OR strategy NOT IN ('convert','reembed','auto') OR reindex NOT IN ('manual','blocking') THEN RAISE EXCEPTION 'invalid migration state or options'; END IF;
    IF r.trigger_mode='none' AND NOT observed_writes_quiesced THEN RAISE EXCEPTION 'observed writes must be quiesced'; END IF;
    IF r.backfill_mode='cursor' THEN RAISE EXCEPTION 'backfill still running; migrate once status() shows backfill_mode done'; END IF;
    SELECT name INTO converter FROM postvec.models WHERE model_type='convert' AND source_model=COALESCE(r.space,r.model) AND target_model=COALESCE((SELECT space FROM postvec._route(new_model)),new_model) ORDER BY (raw->'extra'->>'provider') IS NOT NULL,name LIMIT 1;
    IF strategy='convert' AND converter IS NULL THEN RAISE EXCEPTION 'no direct converter'; END IF;
    via:=CASE WHEN strategy='reembed' OR converter IS NULL THEN jsonb_build_object('kind','reembed') ELSE jsonb_build_object('kind','direct','model',converter) END;
    via:=via || jsonb_build_object('space',COALESCE((SELECT space FROM postvec._route(new_model)),new_model));
    dimension:=COALESCE((SELECT dim FROM postvec._route(new_model) WHERE dim>0),(SELECT target_dim FROM postvec.models WHERE model_type='convert' AND target_model=new_model AND target_dim>0 ORDER BY name LIMIT 1));
    IF dimension IS NULL THEN RAISE EXCEPTION 'target dimension unavailable'; END IF;
    newcol:='postvec_new_'||r.id;
    SELECT format('%I.vector',n.nspname) INTO vt FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='vector';
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN %I %s(%s)',coalesce(r.destination_schema,r.table_schema),coalesce(r.destination_table,r.table_name),newcol,vt,dimension);
    EXECUTE format('SELECT count(*) FROM %I.%I',coalesce(r.destination_schema,r.table_schema),coalesce(r.destination_table,r.table_name)) INTO total;
    INSERT INTO postvec.migrations(registry_id,old_model,new_model,old_dim,new_dim,strategy,resolved_via,new_column,reindex,rows_total) VALUES(r.id,r.model,new_model,r.dim,dimension,strategy,via,newcol,reindex,total) RETURNING id INTO rid;
    UPDATE postvec.registry SET state='migrating' WHERE id=r.id;
    RAISE NOTICE 'Migration inputs are sent to the configured postvec-server inference fleet';
    PERFORM postvec.worker_kick();RETURN rid;
END $$;

CREATE OR REPLACE FUNCTION postvec.migration_abort(migration_id bigint) RETURNS void LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE m postvec.migrations; r postvec.registry;
BEGIN
    SELECT * INTO STRICT m FROM postvec.migrations WHERE id=migration_id;
    SELECT * INTO STRICT r FROM postvec.registry WHERE id=m.registry_id;
    PERFORM postvec._owner(to_regclass(format('%I.%I',r.table_schema,r.table_name)));
    SELECT * INTO STRICT r FROM postvec.registry WHERE id=r.id FOR UPDATE;
    SELECT * INTO STRICT m FROM postvec.migrations WHERE id=migration_id FOR UPDATE;
    IF m.state NOT IN ('running','awaiting_finalize','failed') THEN RAISE EXCEPTION 'migration % is %; only running, awaiting_finalize or failed migrations can abort',m.id,m.state; END IF;
    EXECUTE format('ALTER TABLE %I.%I DROP COLUMN IF EXISTS %I RESTRICT',coalesce(r.destination_schema,r.table_schema),coalesce(r.destination_table,r.table_name),m.new_column);
    UPDATE postvec.migrations SET state='aborted',finished_at=now() WHERE id=m.id;
    UPDATE postvec.registry SET state='active' WHERE id=r.id;
    -- Writes during the migration reached only the new column. Re-embed every
    -- row with the original model; stored vectors stay until replaced, so an
    -- adopted column from a retired model loses nothing.
    IF r.trigger_mode<>'none' THEN
        UPDATE postvec.registry SET backfill_mode='cursor',backfill_watermark=NULL WHERE id=r.id;
        INSERT INTO postvec.settings(key,value) VALUES('backfill_all:'||r.id,'true') ON CONFLICT (key) DO NOTHING;
    END IF;
    PERFORM postvec.worker_kick();
END $$;

CREATE OR REPLACE FUNCTION postvec.migration_finalize(migration_id bigint) RETURNS void LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE m postvec.migrations; r postvec.registry; target regclass; a record; had_index boolean;
BEGIN
    SELECT * INTO STRICT m FROM postvec.migrations WHERE id=migration_id;
    SELECT * INTO STRICT r FROM postvec.registry WHERE id=m.registry_id;
    target:=to_regclass(format('%I.%I',coalesce(r.destination_schema,r.table_schema),coalesce(r.destination_table,r.table_name)));
    -- After the swap only the index is awaited, maybe being built CONCURRENTLY
    -- by the worker right now: an ACCESS EXCLUSIVE lock would deadlock with it.
    IF m.state='awaiting_index' THEN
        PERFORM pg_advisory_xact_lock_shared(1886615158,1);
        IF NOT EXISTS(SELECT FROM pg_class WHERE oid=to_regclass(format('%I.%I',r.table_schema,r.table_name)) AND pg_has_role(current_user,relowner,'USAGE')) THEN
            RAISE EXCEPTION 'table ownership is required';
        END IF;
        SELECT * INTO STRICT m FROM postvec.migrations WHERE id=migration_id FOR UPDATE;
        IF m.state='done' THEN RETURN; END IF;
        IF m.state<>'awaiting_index' OR NOT postvec._has_vector_index(target,r.vector_column) THEN
            RAISE EXCEPTION 'migration % awaits its vector index; see migration_status(%).suggested_index_sql',m.id,m.id;
        END IF;
        UPDATE postvec.migrations SET state='done',finished_at=now() WHERE id=m.id;
        RETURN;
    END IF;
    PERFORM postvec._owner(to_regclass(format('%I.%I',r.table_schema,r.table_name)));
    EXECUTE format('LOCK TABLE %s IN ACCESS EXCLUSIVE MODE',target);
    SELECT * INTO STRICT r FROM postvec.registry WHERE id=r.id FOR UPDATE;
    SELECT * INTO STRICT m FROM postvec.migrations WHERE id=migration_id FOR UPDATE;
    IF m.state NOT IN ('awaiting_finalize','awaiting_index') OR EXISTS(SELECT FROM postvec.jobs WHERE registry_id=r.id) THEN RAISE EXCEPTION 'migration has not drained'; END IF;
    had_index:=postvec._has_vector_index(target,r.vector_column);
    IF m.state='awaiting_index' THEN
        IF NOT had_index THEN RAISE EXCEPTION 'migration % awaits its vector index; see migration_status(%).suggested_index_sql',m.id,m.id; END IF;
        UPDATE postvec.migrations SET state='done',finished_at=now() WHERE id=m.id; RETURN;
    END IF;
    FOR a IN SELECT attr.*,t.typstorage FROM pg_attribute attr JOIN pg_type t ON t.oid=attr.atttypid
        WHERE attr.attrelid IN (SELECT target UNION SELECT relid FROM pg_partition_tree(target))
          AND attr.attname=r.vector_column AND NOT attr.attisdropped LOOP
        IF a.attnotnull OR a.atthasdef OR a.attgenerated<>'' OR a.attidentity<>'' OR a.attacl IS NOT NULL
           OR coalesce(a.attstattarget,-1)<>-1 OR a.attstorage<>a.typstorage OR a.attcompression<>'' OR a.attoptions IS NOT NULL
           OR col_description(a.attrelid,a.attnum) IS NOT NULL
           OR EXISTS(SELECT FROM pg_constraint WHERE conrelid=a.attrelid AND a.attnum=ANY(conkey))
           OR EXISTS(SELECT FROM pg_seclabel WHERE classoid='pg_class'::regclass AND objoid=a.attrelid AND objsubid=a.attnum)
           OR EXISTS(SELECT FROM pg_depend WHERE classid='pg_statistic_ext'::regclass AND refclassid='pg_class'::regclass AND refobjid=a.attrelid AND refobjsubid=a.attnum) THEN
            RAISE EXCEPTION 'old vector column on % has metadata or constraints; resolve before cutover',a.attrelid::regclass;
        END IF;
    END LOOP;
    EXECUTE format('ALTER TABLE %s DROP COLUMN %I RESTRICT',target,r.vector_column);
    EXECUTE format('ALTER TABLE %s RENAME COLUMN %I TO %I',target,m.new_column,r.vector_column);
    UPDATE postvec.registry SET state='active',model=m.new_model,space=COALESCE(m.resolved_via->>'space',(SELECT space FROM postvec._route(m.new_model))),dim=m.new_dim,owns_vector_column=true WHERE id=r.id;
    IF m.reindex='blocking' THEN PERFORM postvec.create_vector_index(format('%I.%I',r.table_schema,r.table_name)::regclass,r.source_column);
    ELSIF had_index THEN
        UPDATE postvec.migrations SET state='awaiting_index' WHERE id=m.id;
        RAISE NOTICE 'migration % awaits its vector index: build it (migration_status(%).suggested_index_sql) and call migration_finalize again, or let a worker with index_mode => auto finish it',m.id,m.id;
        RETURN;
    END IF;
    UPDATE postvec.migrations SET state='done',finished_at=now() WHERE id=m.id;
END $$;

CREATE OR REPLACE FUNCTION postvec.disable(relation regclass,column_name text,drop_column boolean DEFAULT false,drop_destination boolean DEFAULT false) RETURNS void LANGUAGE plpgsql SET search_path=pg_catalog AS $$
DECLARE r postvec.registry; t record;
BEGIN
    PERFORM postvec._owner(relation::regclass);
    SELECT * INTO STRICT r FROM postvec.registry WHERE to_regclass(format('%I.%I',table_schema,table_name))=relation::regclass AND source_column=column_name FOR UPDATE;
    IF r.state='migrating' THEN RAISE EXCEPTION 'abort or finalize migration first'; END IF;
    IF drop_destination THEN RAISE EXCEPTION 'retain destination and drop it explicitly after reviewing dependencies'; END IF;
    FOR t IN SELECT tgname FROM pg_trigger WHERE tgrelid=relation::regclass AND tgname IN ('postvec_sync_'||r.id,'postvec_trunc_'||r.id,'postvec_ins_'||r.id,'postvec_upd_'||r.id,'postvec_del_'||r.id,'postvec_pk_'||r.id) LOOP EXECUTE format('DROP TRIGGER %I ON %s',t.tgname,relation::regclass); END LOOP;
    IF drop_column AND r.owns_vector_column AND r.chunking='none' THEN EXECUTE format('ALTER TABLE %s DROP COLUMN %I RESTRICT',relation::regclass,r.vector_column); END IF;
    DELETE FROM postvec.registry WHERE id=r.id; DELETE FROM postvec.jobs_dead WHERE registry_id=r.id;
    DELETE FROM postvec.settings WHERE key IN ('backfill_all:'||r.id,'index:'||r.id);
END $$;

-- The extension's enqueue triggers insert as the writing role; the managed
-- ones are SECURITY DEFINER, so PUBLIC needs no way into the queue.
REVOKE INSERT ON postvec.jobs FROM PUBLIC;
DO $$ DECLARE f record; BEGIN
    FOR f IN SELECT oid::regprocedure AS signature FROM pg_proc WHERE pronamespace='postvec'::regnamespace AND proname IN ('_owner','_refs','_format','_existing','_register','_enqueue','enable','adopt','set_format','create_vector_index','migrate','migration_abort','migration_finalize','disable') LOOP
        EXECUTE format('REVOKE ALL ON FUNCTION %s FROM PUBLIC',f.signature);
    END LOOP;
END $$;

DO $$ DECLARE r record; pred text; BEGIN
    FOR r IN SELECT reg.* FROM postvec.registry reg JOIN pg_trigger t
        ON t.tgrelid=to_regclass(format('%I.%I',reg.table_schema,reg.table_name))
        AND t.tgname='postvec_pk_'||reg.id AND t.tgfoid='postvec._enqueue()'::regprocedure
        WHERE t.tgattr<>''::int2vector LOOP
        SELECT string_agg(format('OLD.%I IS DISTINCT FROM NEW.%I',c,c),' OR ') INTO pred FROM unnest(r.pk_columns) c;
        EXECUTE format('DROP TRIGGER %I ON %I.%I','postvec_pk_'||r.id,r.table_schema,r.table_name);
        EXECUTE format('CREATE TRIGGER %I AFTER UPDATE ON %I.%I FOR EACH ROW WHEN (%s) EXECUTE FUNCTION postvec._enqueue(%L)',
            'postvec_pk_'||r.id,r.table_schema,r.table_name,pred,r.id);
    END LOOP;
END $$;
