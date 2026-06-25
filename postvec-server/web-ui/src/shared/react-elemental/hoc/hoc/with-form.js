import React, { Component } from 'react';

/**
 * HOC for managing setting and retrieving state from form field inputs.
 *
 * @param {Function} initial Function describing how to derive initial form state from component
 *                           props. Default to a thunk that produces an empty map.
 * @param {Function} changeEventValue Function that accepts a change event and returns the
 *                   corresponding value for the change. If omitted, the default implementation
 *                   works well with most native browser form fields.
 * @returns {Function} HOC factory that accepts a component and returns an HOC with form and form
 *                     handler injected props. The form represents the current state of all tracked
 *                     values while the form handler is a factory that accepts a form key and
 *                     returns a value change handler function.
 */
const withForm = ({
  initial = () => ({}),
  changeEventValue = (evt) => (evt.target ? evt.target.value : evt)
} = {}) => (WrappedComponent) => {
  return class WithFormHOC extends Component {
    constructor(...args) {
      super(...args);
      this.state = initial(this.props);
    }

    handleFormChange = (key) => (evt) => {
      this.setState({ [key]: changeEventValue(evt) });
    };

    render() {
      const props = {
        ...this.props,
        form: this.state,
        handleFormChange: this.handleFormChange
      };

      return <WrappedComponent {...props} />;
    }
  };
};

export default withForm;