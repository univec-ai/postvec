import React, { useEffect } from 'react'
import { useStore } from '@/store'
import Header from './Header'
import PageHome from './PageHome'
import { Elemental } from '../shared/react-elemental'
import {
  karlaBold,
  karlaRegular,
  sourceCodeProMedium,
  sourceCodeProRegular,
} from '../shared/react-elemental-fonts/index-esm.js'

const REFRESH_MS = 30_000

const App = () => {
  const getConfiguration = useStore((state) => state.getConfiguration)

  useEffect(() => {
    getConfiguration()
    const timer = setInterval(getConfiguration, REFRESH_MS)
    return () => clearInterval(timer)
  }, [getConfiguration])

  return (
    <Elemental
      fontOpts={{
        primary: { regular: karlaRegular, bold: karlaBold },
        secondary: { regular: sourceCodeProRegular, bold: sourceCodeProMedium },
      }}
      colorOpts={{ primary: '#228165', primaryLight: '#d4efe6', primaryDark: '#145a47' }}
    >
      <Header />
      <PageHome />
    </Elemental>
  )
}

export default App
