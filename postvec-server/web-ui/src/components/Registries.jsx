import React, { useEffect, useRef, useState } from 'react'
import { useStore } from '@/store'

const bytes = (n) => {
  if (n == null || n === '') return ''
  const v = Number(n)
  if (!Number.isFinite(v)) return ''
  if (v < 1024) return `${v} B`
  if (v < 1024 ** 2) return `${(v / 1024).toFixed(1)} KiB`
  if (v < 1024 ** 3) return `${(v / 1024 ** 2).toFixed(1)} MiB`
  return `${(v / 1024 ** 3).toFixed(1)} GiB`
}

const licenseToken = (model) =>
  `${model.license || ''}@${model.license_version || ''}`

const licenseTerms = (model, models) => {
  const byName = new Map(models.map((item) => [item.name, item]))
  const seen = new Set()
  const terms = new Map()
  const visit = (item) => {
    if (!item || seen.has(item.name)) return
    seen.add(item.name)
    for (const dependency of item.dependencies || []) visit(byName.get(dependency))
    if (item.license_acceptance === 'notice') terms.set(licenseToken(item), item)
  }
  visit(model)
  return [...terms.values()]
}

const Status = ({ value, ok }) => (
  <span className={`status${ok === undefined ? '' : ok ? ' ok' : ' err'}`}>{value}</span>
)

const matches = (model, query) =>
  [model.name, model.summary, model.backend, model.model_type, model.license]
    .filter(Boolean)
    .some((value) => value.toLowerCase().includes(query))

const count = (shown, all) => (shown === all ? shown : `${shown} of ${all}`)
const filtered = (models, query) =>
  models.filter((model) => matches(model, query)).sort((a, b) => a.name.localeCompare(b.name))

const Registries = () => {
  const registry = useStore((state) => state.registry)
  const manage = useStore((state) => !!state.server?.manage)
  const { loadRegistry, loadPulls, manageModels, setRegistryKey } = useStore.getState()
  const [keyDraft, setKeyDraft] = useState('')
  const [filter, setFilter] = useState('')
  const [pending, setPending] = useState(null)
  const [accepted, setAccepted] = useState(false)
  const wasRunning = useRef(false)

  useEffect(() => {
    loadRegistry()
  }, [loadRegistry])

  const running = registry.pulls.some((job) => job.status === 'running' || job.status === 'queued')
  useEffect(() => {
    if (!running) {
      if (wasRunning.current) loadRegistry()
      wasRunning.current = false
      return
    }
    wasRunning.current = true
    const timer = setInterval(loadPulls, 1000)
    return () => clearInterval(timer)
  }, [running, loadPulls, loadRegistry])

  const q = filter.toLowerCase()
  const installed = filtered(registry.installed, q)
  const available = filtered(registry.available, q)
  const activePulls = registry.pulls.filter((job) => ['queued', 'running'].includes(job.status))
  const busy = registry.busy

  const onPull = (model) => {
    const terms = licenseTerms(model, registry.available)
    if (terms.length > 0) {
      setPending({ model, terms })
      setAccepted(false)
      return
    }
    manageModels('pull', [model.name])
  }

  const confirmPull = () => {
    if (!pending || !accepted) return
    manageModels('pull', [pending.model.name], { accept_license: pending.terms.map(licenseToken) })
    setPending(null)
  }

  const confirmed = (kind, name, question) => () =>
    window.confirm(question) && manageModels(kind, [name])

  return (
    <div className="registry">
      <div className="reg-head">
        <div>
          <h1>Model registries</h1>
          <p className="hint">
            A pull lands deactivated; activate to load it. Remove deletes it from disk.
            {!manage && ' Management is read-only: restart this node with --manage to enable changes.'}
          </p>
        </div>
        <div className="reg-actions">
          <input
            type="search"
            placeholder="Filter"
            aria-label="Filter installed models and catalogue"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
          <button className="btn" onClick={loadRegistry} disabled={registry.loading}>
            {registry.loading ? 'Refreshing…' : 'Refresh'}
          </button>
        </div>
      </div>

      {registry.error && <div className="banner">{registry.error}</div>}

      <div className="reg-key">
        {registry.key ? (
          <>
            <span>
              <b>{registry.authenticated ? 'Private catalogue' : 'Catalogue'}</b> · using your API
              key {registry.signed_in_as ? `(${registry.signed_in_as}) ` : ''}for this tab only. It
              is not stored on this node and is forgotten when the tab closes.
            </span>
            <button
              className="btn"
              onClick={() => {
                setRegistryKey('')
                loadRegistry()
              }}
            >
              Forget key
            </button>
          </>
        ) : registry.signed_in_as ? (
          <span>
            <b>Private catalogue</b> · this node signs in with its own credential (
            {registry.signed_in_as}).
          </span>
        ) : (
          <>
            <span>
              <b>Public catalogue.</b> Add your UniVec API key to browse and pull the private
              models your account is entitled to. It is used from this tab only and never stored
              on the node.{' '}
              <a href="https://univec.ai/dashboard" target="_blank" rel="noreferrer">
                Get a key
              </a>
            </span>
            <form
              className="reg-actions"
              onSubmit={(e) => {
                e.preventDefault()
                if (!keyDraft.trim()) return
                setRegistryKey(keyDraft)
                setKeyDraft('')
                loadRegistry()
              }}
            >
              <input
                type="password"
                placeholder="uv_…"
                aria-label="UniVec API key"
                value={keyDraft}
                onChange={(e) => setKeyDraft(e.target.value)}
                autoComplete="off"
              />
              <button className="btn" type="submit" disabled={!keyDraft.trim()}>
                Use key
              </button>
            </form>
          </>
        )}
      </div>

      {pending && (
        <div className="license">
          <div>
            <b>{pending.model.name}</b> requires acknowledgement of{' '}
            {pending.terms.map((term, index) => (
              <React.Fragment key={licenseToken(term)}>
                {index > 0 && ', '}
                {term.license_url ? (
                  <a href={term.license_url} target="_blank" rel="noreferrer">
                    {licenseToken(term)}
                  </a>
                ) : (
                  licenseToken(term)
                )}
              </React.Fragment>
            ))}
            . Pulling records the acknowledgement on this node.
          </div>
          <label>
            <input type="checkbox" checked={accepted} onChange={(e) => setAccepted(e.target.checked)} />
            I accept these terms
          </label>
          <div className="reg-actions">
            <button className="btn" onClick={() => setPending(null)}>
              Cancel
            </button>
            <button className="btn primary" disabled={!accepted} onClick={confirmPull}>
              Pull
            </button>
          </div>
        </div>
      )}

      {registry.pulls.length > 0 && (
        <section className="reg-block">
          <h2>Pulls</h2>
          {registry.pulls
            .slice()
            .reverse()
            .map((job) => {
              const total = job.total_bytes || 0
              const pct = total ? Math.min(100, Math.round((100 * (job.downloaded_bytes || 0)) / total)) : 0
              return (
                <div className="pull" key={job.id}>
                  <div className="pull-line">
                    <span>
                      #{job.id} · {job.models.join(', ')}
                    </span>
                    <Status
                      value={job.status}
                      ok={job.status === 'done' ? true : job.status === 'failed' ? false : undefined}
                    />
                  </div>
                  {(job.status === 'running' || job.status === 'queued') && (
                    <div className="mem-bar">
                      <div className="mem-fill" style={{ width: `${pct}%` }} />
                    </div>
                  )}
                  <div className="hint">
                    {bytes(job.downloaded_bytes)}
                    {total ? ` / ${bytes(total)} · ${pct}%` : ''}
                    {job.error ? ` · ${job.error}` : ''}
                    {(job.results || [])
                      .map((r) => ` · ${r.model}: ${r.status}${r.error ? ` (${r.error})` : ''}`)
                      .join('')}
                  </div>
                </div>
              )
            })}
        </section>
      )}

      <section className="reg-block">
        <h2>Installed · {count(installed.length, registry.installed.length)}</h2>
        {installed.length === 0 ? (
          <p className="empty">
            {registry.installed.length
              ? 'No installed models match the filter.'
              : 'Nothing on disk yet. Pull from the catalogue, or copy a model tree in.'}
          </p>
        ) : (
          <table className="reg-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Type</th>
                <th>Dim</th>
                <th>Size</th>
                <th>State</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {installed.map((m) => (
                <tr key={m.name}>
                  <td>
                    <div className="model-row-name">{m.name}</div>
                    <div className="model-row-meta">
                      {m.backend}
                      {m.revision != null && ` · rev ${m.revision}`}
                      {m.owner && ` · ${m.owner}`}
                      {m.receipt_error && ` · receipt: ${m.receipt_error}`}
                    </div>
                  </td>
                  <td>{m.model_type || '—'}</td>
                  <td>{m.target_dim || '—'}</td>
                  <td>{bytes(m.disk_bytes)}</td>
                  <td>
                    {m.loaded ? (
                      <Status value="loaded" ok />
                    ) : (
                      <span className="muted">{m.enabled ? 'not loaded' : 'deactivated'}</span>
                    )}
                  </td>
                  <td className="reg-row-actions">
                    {m.enabled && !m.loaded && (
                      <button
                        className="btn"
                        disabled={!manage || !!busy}
                        onClick={() => manageModels('activate', [m.name])}
                      >
                        {busy?.kind === 'activate' && busy.name === m.name ? 'Loading…' : 'Load'}
                      </button>
                    )}{' '}
                    {m.enabled ? (
                      <button
                        className="btn"
                        disabled={!manage || !!busy}
                        onClick={confirmed(
                          'deactivate',
                          m.name,
                          `Deactivate ${m.name}? It will be unloaded from this node.`,
                        )}
                      >
                        {busy?.kind === 'deactivate' && busy.name === m.name
                          ? 'Deactivating…'
                          : 'Deactivate'}
                      </button>
                    ) : (
                      <button
                        className="btn primary"
                        disabled={!manage || !!busy}
                        onClick={() => manageModels('activate', [m.name])}
                      >
                        {busy?.kind === 'activate' && busy.name === m.name ? 'Activating…' : 'Activate'}
                      </button>
                    )}{' '}
                    {m.removable ? (
                      <button
                        className="btn danger"
                        disabled={!manage || !!busy}
                        onClick={confirmed(
                          'remove',
                          m.name,
                          `Remove ${m.name} from this node? ${bytes(m.disk_bytes)} on disk will be deleted.`,
                        )}
                      >
                        {busy?.kind === 'remove' && busy.name === m.name ? 'Removing…' : 'Remove'}
                      </button>
                    ) : null}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section className="reg-block">
        <h2>Catalogue · {count(available.length, registry.available.length)}</h2>
        {available.length === 0 ? (
          <p className="empty">
            {registry.loading
              ? 'Loading the catalogue…'
              : 'No catalogue entries (or none match the filter).'}
          </p>
        ) : (
          <table className="reg-table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Type</th>
                <th>Dim</th>
                <th>Size</th>
                <th>License</th>
                <th>Status</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {available.map((m) => {
                const pull = activePulls.find((job) => job.models.includes(m.name))
                const pulling = busy?.kind === 'pull' && busy.name === m.name
                const canPull = m.update === 'not-installed' && !m.withdrawn
                return (
                  <tr key={m.name} className={m.withdrawn ? 'dimmed' : ''}>
                    <td>
                      <div className="model-row-name">{m.name}</div>
                      {m.summary && <div className="model-row-meta">{m.summary}</div>}
                    </td>
                    <td>{m.model_type || '—'}</td>
                    <td>{m.target_dim || '—'}</td>
                    <td>{bytes(m.download_bytes)}</td>
                    <td>{m.license || '—'}</td>
                    <td>
                      {m.withdrawn
                        ? 'withdrawn'
                        : m.update === 'upgradable'
                          ? `rev ${m.installed_revision} → ${m.revision}`
                          : m.update === 'current'
                            ? 'installed'
                            : m.update === 'unknown'
                              ? 'on disk'
                              : 'not installed'}
                    </td>
                    <td className="reg-row-actions">
                      {canPull ? (
                        <button
                          className="btn primary"
                          disabled={!manage || !!busy || !!pull}
                          onClick={() => onPull(m)}
                        >
                          {pulling ? 'Starting…' : pull?.status === 'queued' ? 'Queued' : pull ? 'Pulling…' : 'Pull'}
                        </button>
                      ) : m.update === 'upgradable' ? (
                        <span className="hint">upgrade via CLI</span>
                      ) : null}
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        )}
      </section>
    </div>
  )
}

export default Registries
