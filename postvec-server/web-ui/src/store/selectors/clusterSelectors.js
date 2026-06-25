export const getClusterMembers = (state) => {
  const cluster = state.cluster || []
  const fromCluster = cluster
    .map((node) => node.frontend_address || node.address)
    .filter((address) => address && address.length > 0)
  if (fromCluster.length > 0) {
    return fromCluster
  }
  // Single-node with no gossip still has a query target: this origin.
  return state.api_endpoint ? [state.api_endpoint] : []
}

export const getClusterGroups = (state) => {
  const cluster = state.cluster || []
  const groups = {}
  for (const node of cluster) {
    const group = node.group || 'postvec'
    if (!groups[group]) {
      groups[group] = []
    }
    groups[group].push({
      address: node.frontend_address || node.address,
      isCurrent: !!node.current,
    })
  }
  for (const group of Object.keys(groups)) {
    groups[group].sort((a, b) => a.address.localeCompare(b.address))
  }
  return groups
}

export const getClusterQueryPeersActive = (state) => {
  const queryPeers = state.prefs.query_peers || []
  const clusterMembers = getClusterMembers(state)
  const members = clusterMembers.reduce((acc, curr) => {
    acc[curr] = true
    return acc
  }, {})
  const selectedPeers = queryPeers.filter((peer) => members.hasOwnProperty(peer))
  if (selectedPeers.length > 0) {
    return selectedPeers
  }
  if (clusterMembers.length > 0) {
    return [clusterMembers[0]]
  }
  return []
}
