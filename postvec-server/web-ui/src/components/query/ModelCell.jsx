import React from 'react'
import { Text, colors } from '../../shared/react-elemental'

const trim = (str, max) => (str.length > max ? `${str.substring(0, max)}..` : str)

const ModelCell = ({ model, active, onClick }) => {
  const configuration = model.configuration || {}
  const backend = configuration.backend || model.provider || 'provider'
  const modelType = configuration.params?.model_type || 'embed'
  const color = active ? colors.white : colors.gray75
  return (
    <div
      className={`model-item ${active ? 'selected' : 'selectable'}`}
      onClick={() => onClick(model.name)}
    >
      <div className="model-item-content">
        <Text size="kilo" bold color={color} className="model-item-main-name">
          {trim(model.name, 48).toUpperCase()}
        </Text>
        <div className="model-item-labels">
          <span className="label" style={{ backgroundColor: colors.primaryDark }}>
            {String(backend).toLowerCase()}
          </span>
          <span className="label" style={{ backgroundColor: colors.primary }}>
            {modelType}
          </span>
        </div>
      </div>
    </div>
  )
}

export default ModelCell
