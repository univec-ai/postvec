import React from 'react'
import PropTypes from 'prop-types'
import BaseComponent from '../BaseComponent'
import ModelCell from './ModelCell'
import ReactList from 'react-list'
import { colors, Text } from '../../shared/react-elemental'

class ModelsList extends BaseComponent {
  constructor(props) {
    super(props)
    this._bind('renderItem')
  }

  renderItem(index, key) {
    const active = this.props.current === this.props.models[index].name
    return (
      <ModelCell
        key={key}
        onClick={this.props.onChange}
        active={active}
        model={this.props.models[index]}
      />
    )
  }

  render() {
    const count = this.props.models instanceof Array ? this.props.models.length : 0
    if (count === 0) {
      return (
        <div className="m-t-20 m-b-20">
          <Text size="kilo" color={colors.black}>
            No match.
          </Text>
        </div>
      )
    }
    return (
      <div
        style={{ maxHeight: this.props.maxHeight, overflow: 'auto' }}
        className="models-list model-list-body"
      >
        <ReactList
          initialIndex={0}
          itemRenderer={this.renderItem}
          length={count}
          useTranslate3d={true}
        />
      </div>
    )
  }
}

ModelsList.propTypes = {
  maxHeight: PropTypes.number,
  onChange: PropTypes.func,
  models: PropTypes.array,
  current: PropTypes.string,
}

ModelsList.defaultProps = {
  maxHeight: 500,
  onChange: () => false,
  models: [],
  current: '',
}

export default ModelsList
