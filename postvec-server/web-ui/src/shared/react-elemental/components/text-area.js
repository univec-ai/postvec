import PropTypes from 'prop-types';
import React from 'react';
import { withForwardedRef } from '../hoc';
import {TextField} from './text-field';
import { transitionStyle } from '../styles/transition';

/**
 * Styled textarea element for blobs of text input.
 *
 * This component behaves similarly to TextField, with some minor modifications.
 */
const Area = ({ error, secondary, style: overrides, forwardedRef, ...proxyProps }) => {
  const style = {
    ...transitionStyle('border'),
    ...overrides,
  };

  return (
    <TextField
      ref={forwardedRef}
      error={error}
      secondary={secondary}
      style={style}
      {...proxyProps}
      textarea
    />
  );
};

Area.propTypes = {
  // Error string, if the input contents are invalid. This will use a dedicated error style.
  error: PropTypes.string,
  // True to use the secondary component variant.
  secondary: PropTypes.bool,
  // Optional style overrides.
  style: PropTypes.object,
  // HOC props
  forwardedRef: PropTypes.oneOfType([
    PropTypes.shape({ current: PropTypes.instanceOf(Element) }),
    PropTypes.func,
  ]),
};

Area.defaultProps = {
  error: null,
  secondary: false,
  style: {},
  forwardedRef: null,
};

export const TextArea = withForwardedRef(Area);
