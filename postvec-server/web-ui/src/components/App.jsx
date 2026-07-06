import React, { useEffect } from 'react'
import { useStore } from '@/store'
import Sidebar from './Sidebar'
import Workbench from './Workbench'
import Registries from './Registries'

const REFRESH_MS = 30_000

const App = () => {
  const getConfiguration = useStore((state) => state.getConfiguration)
  const error = useStore((state) => state.error)
  const tab = useStore((state) => state.prefs.main_menu_tab)

  useEffect(() => {
    getConfiguration()
    const timer = setInterval(getConfiguration, REFRESH_MS)
    return () => clearInterval(timer)
  }, [getConfiguration])

  return (
    <div className="shell">
      <Sidebar />
      <main className="main">
        {error && <div className="banner">{error}</div>}
        {tab === 'registries' ? <Registries /> : <Workbench />}
      </main>
    </div>
  )
}

export default App
