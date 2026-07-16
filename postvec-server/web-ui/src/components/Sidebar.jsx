import React from 'react'
import { useStore, getFilteredModels, getCurrentModel, isEmbedModel } from '@/store'

const gib = (bytes) => (bytes / 1024 ** 3).toFixed(1)

const Nodes = () => {
  const cluster = useStore((state) => state.cluster)
  const system = useStore((state) => state.system)
  const pct = system?.memory_total_bytes
    ? Math.round((system.memory_used_bytes / system.memory_total_bytes) * 100)
    : null
  return (
    <div className="nodes">
      {cluster.length === 0 && (
        <div className="node-line">
          <span className="dot off" /> not connected
        </div>
      )}
      {cluster.map((node) => {
        const address = node.frontend_address || node.address
        return (
          <div className="node-line" key={address} title={node.group}>
            <span
              className={`dot${String(node.status).toLowerCase() === 'alive' ? '' : ' off'}`}
              role="img"
              aria-label={node.status}
            />
            {node.current ? <span>{address} · this node</span> : <a href={address}>{address}</a>}
          </div>
        )
      })}
      {pct !== null && (
        <div className="mem">
          RAM {gib(system.memory_used_bytes)} / {gib(system.memory_total_bytes)} GiB
          <div className="mem-bar">
            <div
              className={`mem-fill${pct >= 90 ? ' critical' : pct >= 75 ? ' warn' : ''}`}
              style={{ width: `${pct}%` }}
            />
          </div>
        </div>
      )}
    </div>
  )
}

const ModelRow = ({ model, active, onClick }) => {
  const cfg = model.configuration || {}
  const params = cfg.params || {}
  const meta = [
    cfg.backend || model.provider || 'provider',
    isEmbedModel(model) ? 'embed' : 'convert',
    params.target_dim && `${params.target_dim}d`,
  ].filter(Boolean)
  return (
    <button className={`model-row${active ? ' active' : ''}`} onClick={onClick}>
      <div className="model-row-name">{model.name}</div>
      <div className="model-row-meta">
        {meta.map((m) => (
          <span key={m}>{m}</span>
        ))}
      </div>
    </button>
  )
}

const Sidebar = () => {
  const tab = useStore((state) => state.prefs.main_menu_tab)
  const filter = useStore((state) => state.prefs.filter)
  const models = useStore(getFilteredModels)
  const current = useStore(getCurrentModel)
  const total = useStore((state) => state.models.length)
  const server = useStore((state) => state.server)
  const loading = useStore((state) => state.loading)
  const { setMainMenuTab, setFilter, setActiveModel, getConfiguration } = useStore.getState()

  return (
    <aside className="sidebar">
      <div className="brand">
        <img className="brand-mark" src="/favicon.svg" alt="" />
        <span className="brand-name">postvec</span>
        <span className="brand-sub">server{server?.version ? ` ${server.version}` : ''}</span>
      </div>
      <nav className="nav" aria-label="Dashboard sections">
        {[
          ['queries', 'Query'],
          ['registries', 'Registries'],
        ].map(([value, label]) => (
          <button
            key={value}
            className={`nav-item${tab === value ? ' active' : ''}`}
            aria-current={tab === value ? 'page' : undefined}
            onClick={() => setMainMenuTab(value)}
          >
            {label}
          </button>
        ))}
      </nav>
      <div className="sidebar-section">Node</div>
      <Nodes />
      <div className="sidebar-section">Models · {total}</div>
      <div className="filter">
        <input
          type="search"
          placeholder="Filter models"
          aria-label="Filter loaded models"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
      </div>
      <div className="model-list">
        {models.length === 0 && (
          <div className="empty">
            {total === 0 ? 'No model is loaded on this node.' : 'No model matches the filter.'}
          </div>
        )}
        {models.map((model) => (
          <ModelRow
            key={model.name}
            model={model}
            active={model.name === current?.name}
            onClick={() => setActiveModel(model.name)}
          />
        ))}
      </div>
      <div className="sidebar-foot">
        <button className="link" onClick={getConfiguration} disabled={loading}>
          {loading ? 'Refreshing…' : 'Refresh'}
        </button>
        {' · '}auto every 30s
      </div>
    </aside>
  )
}

export default Sidebar
