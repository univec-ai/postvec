import React from 'react'
import ModelCell from './ModelCell'
import { Text } from '../../shared/react-elemental'

const ModelsList = ({ models, current, onChange, maxHeight = 500 }) => {
  if (models.length === 0) {
    return (
      <div className="m-t-20 m-b-20">
        <Text size="kilo">No match.</Text>
      </div>
    )
  }
  return (
    <div style={{ maxHeight, overflow: 'auto' }} className="models-list">
      {models.map((model) => (
        <ModelCell
          key={model.name}
          model={model}
          active={model.name === current}
          onClick={onChange}
        />
      ))}
    </div>
  )
}

export default ModelsList
