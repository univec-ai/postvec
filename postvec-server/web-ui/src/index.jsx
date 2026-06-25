import React from 'react'
import { createRoot } from 'react-dom/client'
import App from './components/App'
import './styles/postvec.css'

if (document) {
  document.addEventListener('DOMContentLoaded', () => {
    const domElement = document.getElementById('react-root')
    if (domElement) {
      const root = createRoot(domElement)
      root.render(
        <React.StrictMode>
          <App />
        </React.StrictMode>,
      )
    } else {
      console.error('Dom element not found: react-root')
    }
  })
}
