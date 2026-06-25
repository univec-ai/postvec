import React, { forwardRef } from 'react';

/**
 * HOC factory for forwarding a ref as a prop in a wrapped component.
 *
 * @param {Component|Function} WrappedComponent Class-based React component.
 * @returns {Component} Component factory with a ref forwarded as injected child prop forwardedRef.
 */
const withForwardedRef = (WrappedComponent) => {
  return forwardRef((props, ref) => (
    <WrappedComponent {...props} forwardedRef={ref} />
  ));
};

export default withForwardedRef;