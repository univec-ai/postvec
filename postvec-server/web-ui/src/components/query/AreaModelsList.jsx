import React from 'react'
import { useStore } from '@/store'
import {
  getFilteredEnabledModels,
  getCurrentModel,
  getClusterMembers,
  getClusterQueryPeersActive,
} from '@/store'
import { Spacing, Text, TextField, Checkbox, colors } from '../../shared/react-elemental'
import ModelsList from './ModelsList'

const AreaModelsList = ({ showLivePeers = false }) => {
  const filter = useStore((state) => state.prefs?.filter)
  const models = useStore(getFilteredEnabledModels)
  const current_model = useStore(getCurrentModel)
  const cluster_members = useStore(getClusterMembers)
  const cluster_query_peers_active = useStore(getClusterQueryPeersActive)
  const setFilterValue = useStore((state) => state.setFilter)
  const setActiveModel = useStore((state) => state.setActiveModel)
  const toggleQueryPeer = useStore((state) => state.toggleQueryPeer)
  const current_name = current_model?.name || ''

  const livePeers = showLivePeers ? (
    <div>
      <Text size="kilo" color={colors.gray30}>
        Execute via node:
      </Text>
      {cluster_members.map((peerName, index) => (
        <div key={`peer-${peerName}-${index}`}>
          <Checkbox
            label={peerName}
            checked={cluster_query_peers_active.includes(peerName)}
            onChange={(value) => toggleQueryPeer(peerName, value)}
          />
        </div>
      ))}
    </div>
  ) : null

  return (
    <div>
      <TextField
        placeholder="Filter models.."
        value={filter}
        onChange={(e) => setFilterValue(e.target.value)}
      />
      <Spacing bottom>
        <ModelsList
          models={models}
          current={current_name}
          onChange={setActiveModel}
          maxHeight={361}
        />
      </Spacing>
      {livePeers}
    </div>
  )
}

export default AreaModelsList
