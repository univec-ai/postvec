import React from 'react'
import PropTypes from 'prop-types'
import BaseComponent from '../BaseComponent'
import { Text, colors } from '../../shared/react-elemental'

class ModelCell extends BaseComponent {
  constructor(props) {
    super(props)
    this._bind('onClick')
  }

  onClick(e) {
    e.stopPropagation()
    this.props.onClick(this.props.model.name)
  }

  trimString(str, maxLen) {
    if (str.length > maxLen) {
      return str.substring(0, maxLen) + '..'
    }
    return str
  }

  render() {
    if (!this.props.model) {
      return null
    }
    const model = this.props.model
    const configuration = model.configuration || {}
    const modelName = configuration.name || model.name || ''
    if (modelName === '') {
      return null
    }
    const rowClass = this.props.active ? 'selected' : 'selectable'
    const backend = configuration.backend || model.provider || 'n/a'
    const modelType = configuration.params?.model_type || 'embed'
    const textColor = this.props.active ? colors.gray80 : colors.gray75
    const modelNameColor = this.props.active ? colors.white : textColor

    return (
      <div className={`model-item ${rowClass}`} onClick={this.onClick}>
        <div className="model-item-content model-item-two-row">
          <div className="model-item-row-1">
            <Text size="kilo" bold color={modelNameColor} className="model-item-main-name">
              {this.trimString(modelName, 40).toUpperCase()}
            </Text>
          </div>
          <div className="model-item-row-2 clearfix fullsize">
            <div className="model-item-backend">
              <Text size="lambda" color={textColor}>
                <span className="label" style={{ backgroundColor: colors.primaryDark }}>
                  {String(backend).toLowerCase()}
                </span>
              </Text>
            </div>
            <div className="model-item-last-updated">
              <Text size="lambda" color={textColor}>
                <span className="label" style={{ backgroundColor: colors.primary }}>
                  {modelType}
                </span>
              </Text>
            </div>
          </div>
        </div>
      </div>
    )
  }
}

ModelCell.propTypes = {
  onClick: PropTypes.func,
  active: PropTypes.bool,
  model: PropTypes.object,
}

ModelCell.defaultProps = {
  onClick: () => false,
  active: false,
  model: null,
}

export default ModelCell
