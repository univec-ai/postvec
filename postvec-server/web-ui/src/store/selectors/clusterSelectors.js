// Query targets. The node that served the page is addressed as this
// origin, so a dashboard opened via localhost or a tunnel keeps working.
// Peers use what they advertise.
export const getClusterMembers = (state) => {
  const members = state.cluster.map((node) =>
    node.current ? state.api_endpoint : node.frontend_address || node.address,
  )
  const unique = [...new Set(members.filter(Boolean))]
  return unique.length > 0 ? unique : [state.api_endpoint]
}

export const getClusterGroups = (state) => {
  const groups = {}
  for (const node of state.cluster) {
    const group = node.group || 'postvec'
    groups[group] ||= []
    groups[group].push({
      address: node.frontend_address || node.address,
      isCurrent: !!node.current,
    })
  }
  for (const group of Object.values(groups)) {
    group.sort((a, b) => a.address.localeCompare(b.address))
  }
  return groups
}

export const getClusterQueryPeersActive = (state) => {
  const members = getClusterMembers(state)
  const selected = state.prefs.query_peers.filter((peer) => members.includes(peer))
  return selected.length > 0 ? selected : members.slice(0, 1)
}
