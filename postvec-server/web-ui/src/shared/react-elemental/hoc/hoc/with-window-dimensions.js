import React, { Component } from 'react';

// Stateful record of added throttled events.
const throttleRecord = {};

/**
 * Attach an event listener to an existing target's event that fires at a throttled rate.
 *
 * @param {Object} target Object onto which the listener should be attached.
 * @param {String} event Name of the original event.
 * @param {String} throttled Name of the throttled event to emit.
 */
export const throttle = (target, event, throttled) => {
  const raf = window.requestAnimationFrame || ((func) => func());

  // Allow idempotent attachment of throttled listeners
  const key = `${event}-${throttled}`;

  if (throttleRecord[key]) {
    return;
  }

  throttleRecord[key] = true;
  let running = false;

  const listener = () => {
    if (running) {
      return;
    }

    running = true;
    raf(() => {
      target.dispatchEvent(new CustomEvent(throttled));
      running = false;
    });
  };

  target.addEventListener(event, listener);
};

/**
 * Higher-order component factory for creating an HOC that supplies the current window width and
 * height to the wrapped child component. It abstracts out logic to responsibly throttle window
 * resize events so to not cause unnecessary slowness in re-rendering.
 *
 * @param {Component|Function} WrappedComponent Component to wrap.
 * @returns {Component} Higher-order component that instantiates the child component with additional
 *                      window width and height props.
 */
const withWindowDimensions = (WrappedComponent) => {
  throttle(window, 'resize', 'optimizedResize');

  return class WithWindowDimensionsHOC extends Component {
    constructor(...args) {
      super(...args);
      this.state = {
        width: null,
        height: null
      };
      this.onResize = this._onResize.bind(this);
      this.animationFrameID = null;
    }

    componentDidMount() {
      window.addEventListener('optimizedResize', this.onResize);
      this.onResize();
    }

    componentWillUnmount() {
      window.removeEventListener('optimizedResize', this.onResize);

      if (this.animationFrameID && window.cancelAnimationFrame) {
        window.cancelAnimationFrame(this.animationFrameID);
      }
    }

    _onResize() {
      const raf = window.requestAnimationFrame || ((func) => func());

      this.animationFrameID = raf(() =>
        this.setState({
          width: window.innerWidth,
          height: window.innerHeight
        })
      );
    }

    render() {
      const { width, height } = this.state;

      if (width === null || height === null) {
        return null;
      }

      return <WrappedComponent {...this.props} width={width} height={height} />;
    }
  };
};

export default withWindowDimensions;