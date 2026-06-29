import React, { useState, useRef, useEffect } from 'react'
import { useStore, getClusterGroups } from '@/store'
import { Button, Text } from '../shared/react-elemental'

const ClusterGroupBadge = ({ groupName, nodes }) => {
  const [open, setOpen] = useState(false)
  const ref = useRef(null)

  useEffect(() => {
    const onClickOutside = (e) => ref.current && !ref.current.contains(e.target) && setOpen(false)
    document.addEventListener('mousedown', onClickOutside)
    return () => document.removeEventListener('mousedown', onClickOutside)
  }, [])

  return (
    <div className="cluster-group-badge" ref={ref}>
      <button className="cluster-group-toggle" onClick={() => setOpen((prev) => !prev)}>
        <span className="cluster-group-name">{groupName}</span>
        <span className="cluster-group-count">{nodes.length}</span>
      </button>
      {open && (
        <div className="cluster-group-menu">
          {nodes.map((node) => (
            <div key={node.address} className="cluster-group-menu-item">
              {node.isCurrent ? (
                <span className="cluster-group-node current">{node.address} (this node)</span>
              ) : (
                <a className="cluster-group-node" href={node.address}>
                  {node.address}
                </a>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

const formatGiB = (bytes) => (bytes / 1024 ** 3).toFixed(1)

const MemoryBadge = ({ system }) => {
  if (!system?.memory_total_bytes) return null
  const { memory_used_bytes: used, memory_total_bytes: total } = system
  const pct = Math.min(100, Math.round((used / total) * 100))
  const level = pct >= 90 ? 'critical' : pct >= 75 ? 'warning' : 'ok'
  return (
    <div
      className={`memory-badge memory-badge-${level}`}
      title={`Host RAM: ${formatGiB(used)} GiB used of ${formatGiB(total)} GiB`}
    >
      <span className="memory-badge-label">RAM</span>
      <div className="memory-badge-bar">
        <div className="memory-badge-fill" style={{ width: `${pct}%` }} />
      </div>
      <span className="memory-badge-text">
        {formatGiB(used)} / {formatGiB(total)} GiB
      </span>
    </div>
  )
}

const Header = () => {
  const system = useStore((state) => state.system)
  const server = useStore((state) => state.server)
  const error = useStore((state) => state.error)
  const loading = useStore((state) => state.loading)
  const clusterGroups = useStore(getClusterGroups)
  const getConfiguration = useStore((state) => state.getConfiguration)
  const groupNames = Object.keys(clusterGroups).sort()

  return (
    <section>
      <div className="container">
        <div className="postvec-header">
          <div className="postvec-title">
            <Text size="iota" uppercase bold className="postvec-wordmark">
              postvec-server
            </Text>
            {server?.version && <span className="postvec-version">v{server.version}</span>}
            <div className="postvec-live-nodes">
              {groupNames.length === 0 ? (
                <span className="postvec-no-nodes">No live nodes</span>
              ) : (
                groupNames.map((name) => (
                  <ClusterGroupBadge key={name} groupName={name} nodes={clusterGroups[name]} />
                ))
              )}
            </div>
          </div>
          <div className="postvec-header-buttons">
            <MemoryBadge system={system} />
            <Button text="Refresh" onClick={getConfiguration} disabled={loading} />
          </div>
        </div>
        {error && <div className="postvec-error">{error}</div>}
      </div>
    </section>
  )
}

export default Header
