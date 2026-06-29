import React from 'react'

const CustomTabs = ({ options, value, onChange, tabClassName = 'tab-item' }) => (
  <div className="custom-tabs">
    {options.map((option) => (
      <button
        key={option.value}
        className={option.value === value ? `${tabClassName} active` : tabClassName}
        onClick={() => onChange(option.value)}
      >
        {option.label}
      </button>
    ))}
  </div>
)

export default CustomTabs
