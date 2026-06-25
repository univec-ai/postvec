import React from 'react';
import { primaryFontStyle, secondaryFontStyle } from '../styles/font';

/**
 * Text component with automatic typeface formatting.
 */
export const Text = ({ 
  secondary = false,
  size = 'iota',
  color = 'gray80',
  bold = false,
  inline = false,
  uppercase = false,
  center = false,
  right = false,
  style: overrides = {},
  children = null,
  ...proxyProps 
}) => {

  const fontStyleFactory = secondary ? secondaryFontStyle : primaryFontStyle;
  const textAlign = (() => {
    if (center) {
      return 'center';
    }
    if (right) {
      return 'right';
    }
    return 'unset';
  })();
  const style = {
    ...fontStyleFactory(size, color, bold),
    textTransform: uppercase ? 'uppercase' : 'none',
    textAlign,
    ...overrides,
  };

  if (inline) {
    return (
      <span style={style} {...proxyProps}>
        {children}
      </span>
    );
  }

  return (
    <p style={style} {...proxyProps}>
      {children}
    </p>
  );
};
