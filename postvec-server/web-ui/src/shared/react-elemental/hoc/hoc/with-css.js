import React, { Component } from 'react';

// Module-scoped store to statefully track the CSS nodes that have been injected into the document.
// The map is keyed by an injection key derived from the client-specified `key` function.
const injectedNodes = {};

/**
 * Inject a CSS style declaration into the document body for a specific key. This function will
 * update the existing CSS declaration if its corresponding key is already present or adds a new CSS
 * node for a newly observed key.
 *
 * @param {String} key Injection key, used to uniquely identify this CSS injection.
 * @param {String} css Valid CSS string.
 */
const statefullyInjectCSS = (key, css) => {
  // Retrieve the style node if the key has already been injected; create and add a new one if it
  // does not yet exist
  const node = injectedNodes[key] || (() => {
    const created = document.createElement('style');
    document.body.appendChild(created);
    injectedNodes[key] = created;
    return created;
  })();

  // Save on style diff computation by updating the style only if it has changed
  if (node.innerHTML !== css) {
    node.innerHTML = css;
  }
};

/**
 * Higher-order component factory that generates an HOC that wraps injection of global CSS into the
 * document head.
 *
 * @param {Function} key Function that accepts current component props and returns a unique key for
 *                       this particular CSS injection. Generally speaking, this function should
 *                       return a constant if the wrapped component will only ever perform one CSS
 *                       injection (and changes in props will update the existing declaration).
 *                       Conversely, it may map to multiple keys if the wrapped component needs to
 *                       retain multiple CSS declarations.
 * @param {Function} css Function that accepts current component props and returns a CSS declaration
 *                       string to inject into the document body. It will either create a new style
 *                       node or update an existing style node, keyed by the return value of the
 *                       keying function. Note that all injected CSS has global scope.
 * @returns {Function} HOC factory that takes a component class or function as a parameter and
 *                     returns an HOC wrapping the specified component.
 */
const withCSS = ({ key, css }) => (WrappedComponent) => {
  return class WithCSSHOC extends Component {
    componentDidMount() {
      this._updateInjectedCSS();
    }

    componentDidUpdate() {
      this._updateInjectedCSS();
    }

    _updateInjectedCSS() {
      statefullyInjectCSS(key(this.props), css(this.props));
    }

    render() {
      return <WrappedComponent {...this.props} />;
    }
  };
};

export default withCSS;