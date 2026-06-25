import React from 'react'
import ReactDOM from 'react-dom'

export default class BaseComponent extends React.Component {
  _bind(...methods) {
    methods.forEach((method) => {
      this[method] = this[method].bind(this)
    })
  }

  getCoordinates(ref) {
    let domElement = null
    if (ref && this.refs.hasOwnProperty(ref)) {
      domElement = ReactDOM.findDOMNode(this.refs[ref])
    } else {
      domElement = ReactDOM.findDOMNode(this)
    }
    return domElement ? domElement.getBoundingClientRect() : {}
  }
}
