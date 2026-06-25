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

const Application = () => {
  const appMounted = useStore((state) => state.appMounted)

  useEffect(() => {
    appMounted()
  }, [appMounted])

  return (
    <Elemental
      fontOpts={{
        primary: { regular: karlaRegular, bold: karlaBold },
        secondary: { regular: sourceCodeProRegular, bold: sourceCodeProMedium },
      }}
      colorOpts={{
        primary: '#228165',
        primaryLight: '#d4efe6',
        primaryDark: '#145a47',
      }}
    >
      <Header />
      <PageHome />
    </Elemental>
  )
}

export default Application
