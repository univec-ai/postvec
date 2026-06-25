import PropTypes from 'prop-types';
import React from 'react';
import {Spacing} from '../spacing';
import {Text} from '../text';
import PrimaryTabOption from './primary-tab-option';
import SecondaryTabOption from './secondary-tab-option';
import noop from '../../util/noop';

/**
 * Horizontally organized segments of options.
 */
export const Tabs = ({
  options,
  value: selected,
  secondary = false,
  fit = false,
  invert = false,
  onChange = noop,
  style: overrides = {},
  ...proxyProps
}) => {
  const containerStyle = {
    alignItems: 'end',
    display: 'flex',
    justifyContent: fit ? 'inherit' : 'space-around',
    ...overrides,
  };

  const buttonIdleStyle = {
    backgroundColor: 'inherit',
    borderRadius: 0,
    cursor: 'pointer',
    textAlign: 'center',
    width: '100%',
  };

  const TabOption = secondary ? SecondaryTabOption : PrimaryTabOption;

  return (
    <div style={containerStyle} {...proxyProps}>
      {options.map(({ value, label }, idx) => (
        <div key={value} style={fit ? {} : { flex: 1 }}>
          <TabOption
            baseStyle={buttonIdleStyle}
            isIntermediate={idx < options.length - 1}
            isSelected={selected === value}
            isInvert={invert}
            onClick={() => onChange(value)}
          >
            {typeof label === 'string' ? (
              <Spacing size="tiny" top bottom padding>
                <Text color="gray60">
                  {label}
                </Text>
              </Spacing>
            ) : label}
          </TabOption>
        </div>
      ))}
    </div>
  );
};

Tabs.propTypes = {
  // Prop types are defined in the function signature
};

