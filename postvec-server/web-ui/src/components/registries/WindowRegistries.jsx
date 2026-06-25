import React from 'react'
import { Text, Spacing } from '../../shared/react-elemental'

const WindowRegistries = () => (
  <div className="postvec-placeholder">
    <Spacing bottom>
      <Text size="kilo" bold>
        Model registries
      </Text>
    </Spacing>
    <Text size="kilo" color="gray40">
      Registry browse, pull and activate land in a later release. This node
      still loads models that are already on disk: `postvec model pull`, then
      `postvec-server load`.
    </Text>
  </div>
)

export default WindowRegistries
