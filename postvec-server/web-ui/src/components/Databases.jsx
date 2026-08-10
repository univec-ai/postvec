// SPDX-License-Identifier: BUSL-1.1
import React, { useState } from 'react'
import { useStore } from '@/store'

export default function Databases() {
  const databases = useStore((s) => s.server?.managed || [])
  const endpoint = useStore((s) => s.api_endpoint)
  const [detail, setDetail] = useState(null)
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const request = async (db, action, body) => {
    setBusy(true)
    setError('')
    try {
      const response = await fetch(`${endpoint}/admin/managed/${encodeURIComponent(db)}/${action}`, {
        method: action === 'jobs' ? 'GET' : 'POST',
        headers: { 'Content-Type': 'application/json' },
        ...(body ? { body: JSON.stringify(body) } : {}),
      })
      if (response.status === 404) throw new Error('These actions are available on the loopback admin port. Open the dashboard through an SSH tunnel to that port.')
      const result = await response.json()
      if (!response.ok) throw new Error(result.error || 'Request failed')
      if (action === 'jobs') setDetail({ db, ...result.data })
      else { setDetail(null); await useStore.getState().getConfiguration() }
    } catch (e) { setError(e.message) } finally { setBusy(false) }
  }
  return <section className="registry">
    <h1>Databases</h1>
    <p>Organization production use requires <a href="https://github.com/univec-ai/postvec/blob/main/LICENSING.md">postvec Pro</a>. Personal noncommercial use, non-production use and one 30-day production evaluation per organization are free.</p>
    {error && <p role="alert" className="hint">{error}</p>}
    {!databases.length && <p className="empty">No managed databases configured. Add a managed block to the server configuration or use serve --sync.</p>}
    {databases.map((db) => {
      const d = db.database || {}
      const age = d.heartbeat_age_seconds
      return <article key={db.name} className="reg-block">
        <h2>{db.name} · {db.leader ? 'leader' : 'standby'}</h2>
        {db.error && <p role="alert">{db.error}</p>}
        <p>{d.platform || 'Connecting'} · schema {d.schema_version ?? '—'} · leader {d.leader || '—'}</p>
        <p>Heartbeat: {age == null ? 'unknown' : `${Math.round(age)}s ago`} · queued: {d.queue_depth ?? '—'} · dead letters: {d.dead_letters ?? '—'}</p>
        <p>Proxy: {db.proxy ? `port ${db.proxy.port} · ${db.proxy.connections} connections · ${db.proxy.rewrites.search} searches, ${db.proxy.rewrites.embed} embeds` : 'not configured'}</p>
        {(d.migrations || []).map((m) => <p key={m.id}>Migration {m.id}: {m.old_model} → {m.new_model} · {m.state} · {m.rows_done} done, {m.rows_skipped} skipped {m.error && `· ${m.error}`}</p>)}
        {d.grant_script && <details><summary>Source-owner grant script</summary><pre>{d.grant_script}</pre></details>}
        <button className="btn" disabled={busy} onClick={() => request(db.name, 'jobs')}>Inspect jobs</button>{' '}
        <button className="btn" disabled={busy} onClick={() => request(db.name, 'refresh-models')}>Refresh models</button>
      </article>
    })}
    {detail && <section><h2>Jobs · {detail.db}</h2>
      <pre>{JSON.stringify({ jobs: detail.jobs, quarantine: detail.quarantine }, null, 2)}</pre>
      {(detail.dead || []).map((j) => <p key={j.dead_id}>Dead job {j.dead_id}: {j.last_error}{' '}
        <button className="btn" disabled={busy} onClick={() => request(detail.db, 'retry-dead', { registry_id: j.registry_id, dead_ids: [j.dead_id] })}>Retry</button>
      </p>)}
    </section>}
  </section>
}
