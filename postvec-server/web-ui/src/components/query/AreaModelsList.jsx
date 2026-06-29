import React from 'react'
import {
  useStore,
  getFilteredModels,
  getCurrentModel,
  getClusterMembers,
  getClusterQueryPeersActive,
} from '@/store'
import { Spacing, Text, TextField, Checkbox, colors } from '../../shared/react-elemental'
import ModelsList from './ModelsList'

const AreaModelsList = () => {
  const filter = useStore((state) => state.prefs.filter)
  const models = useStore(getFilteredModels)
  const current = useStore(getCurrentModel)
  const members = useStore(getClusterMembers)
  const activePeers = useStore(getClusterQueryPeersActive)
  const setFilter = useStore((state) => state.setFilter)
  const setActiveModel = useStore((state) => state.setActiveModel)
  const toggleQueryPeer = useStore((state) => state.toggleQueryPeer)

  return (
    <div>
      <TextField
        placeholder="Filter models.."
        value={filter}
        onChange={(e) => setFilter(e.target.value)}
      />
      <Spacing bottom>
        <ModelsList
          models={models}
          current={current?.name || ''}
          onChange={setActiveModel}
          maxHeight={420}
        />
      </Spacing>
      <Text size="kilo" color={colors.gray30}>
        Execute via node:
      </Text>
      {members.map((peer) => (
        <div key={peer}>
          <Checkbox
            label={peer}
            checked={activePeers.includes(peer)}
            onChange={(value) => toggleQueryPeer(peer, value)}
          />
        </div>
      ))}
    </div>
  )
}

export default AreaModelsList
