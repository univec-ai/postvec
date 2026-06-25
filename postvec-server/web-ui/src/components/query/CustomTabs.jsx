import React, { Component } from 'react'
import PropTypes from 'prop-types'
import { Spacing, Text } from '../../shared/react-elemental'

const withToggleState =
  ({ key, enable, disable }) =>
  (WrappedComponent) =>
    class WithToggleStateHOC extends Component {
      state = { isToggled: false }
      handleToggle = (isToggled) => () => this.setState({ isToggled })
      enable = this.handleToggle(true)
      disable = this.handleToggle(false)
      render() {
        const { isToggled } = this.state
        return (
          <WrappedComponent
            {...this.props}
            {...{ [key]: isToggled, [enable]: this.enable, [disable]: this.disable }}
          />
        )
      }
    }

class PrimaryTabOption extends Component {
  render() {
    const {
      tabClassName,
      isSelected,
      onClick,
      handleMouseEnter,
      handleMouseLeave,
      children,
    } = this.props
    const clazz = isSelected ? `${tabClassName} active` : tabClassName
    return (
      <button
        className={clazz}
        onClick={onClick}
        onMouseEnter={handleMouseEnter}
        onMouseLeave={handleMouseLeave}
      >
        {children}
      </button>
    )
  }
}

PrimaryTabOption.propTypes = {
  tabClassName: PropTypes.string.isRequired,
  isSelected: PropTypes.bool.isRequired,
  onClick: PropTypes.func.isRequired,
  children: PropTypes.node.isRequired,
  handleMouseEnter: PropTypes.func.isRequired,
  handleMouseLeave: PropTypes.func.isRequired,
}

const TabItem = withToggleState({
  key: 'isHover',
  enable: 'handleMouseEnter',
  disable: 'handleMouseLeave',
})(PrimaryTabOption)

class CustomTabs extends Component {
  render() {
    const { tabClassName, options, value: selected, invert, onChange, ...proxyProps } =
      this.props
    return (
      <div {...proxyProps}>
        {options.map(({ value, label }, idx) => (
          <div className="pull-left" key={value}>
            <TabItem
              tabClassName={tabClassName}
              isIntermediate={idx < options.length - 1}
              isSelected={selected === value}
              isInvert={invert}
              onClick={() => onChange(value)}
            >
              {typeof label === 'string' ? (
                <Spacing size="tiny" top bottom padding>
                  <Text color="gray60">{label}</Text>
                </Spacing>
              ) : (
                label
              )}
            </TabItem>
          </div>
        ))}
        <div className="clearfix" />
      </div>
    )
  }
}

export default CustomTabs
