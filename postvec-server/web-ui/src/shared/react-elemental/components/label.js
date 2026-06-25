import React from 'react';
import { Spacing } from './spacing';
import { Text } from './text';

/**
 * Text label accompanying an input field.
 *
 * @constructor
 */
export const Label = ({ label = null, sublabel = null, ...proxyProps }) => (
  <Spacing size="tiny" bottom {...proxyProps}>
    {label && (
      <Text size="kilo" color="gray50" uppercase bold>
        {label}
      </Text>
    )}

    {sublabel && (
      <Text size="lambda" color="gray25">
        {sublabel}
      </Text>
    )}
  </Spacing>
);