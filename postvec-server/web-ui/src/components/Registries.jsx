import React from 'react'

const Registries = () => (
  <div className="page">
    <h1>Model registries</h1>
    <p>
      Browsing, pulling and activating models from a registry lands in a later release. Until
      then this node loads what is already on disk: <code>postvec model pull</code>, then{' '}
      <code>postvec-server load</code>.
    </p>
  </div>
)

export default Registries
