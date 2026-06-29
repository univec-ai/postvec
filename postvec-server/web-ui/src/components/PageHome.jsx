import React from 'react'
import { useStore } from '@/store'
import { Spacing, Tabs, Text } from '../shared/react-elemental'
import WindowQuery from './query/WindowQuery'
import WindowRegistries from './registries/WindowRegistries'

const TabLabel = ({ children }) => (
  <Spacing size="small" left right>
    <Spacing size="tiny" top bottom>
      <Text>{children}</Text>
    </Spacing>
  </Spacing>
)

const PAGES = { queries: WindowQuery, registries: WindowRegistries }

const PageHome = () => {
  const tab = useStore((state) => state.prefs.main_menu_tab)
  const setMainMenuTab = useStore((state) => state.setMainMenuTab)
  const Page = PAGES[tab] || WindowQuery

  return (
    <section className="postvec-page">
      <div className="container">
        <Spacing bottom>
          <Tabs
            options={[
              { value: 'queries', label: <TabLabel>Queries</TabLabel> },
              { value: 'registries', label: <TabLabel>Registries</TabLabel> },
            ]}
            value={Page === WindowQuery ? 'queries' : tab}
            onChange={setMainMenuTab}
            secondary
            fit
          />
        </Spacing>
        <div className="postvec-workspace">
          <Page />
        </div>
      </div>
    </section>
  )
}

export default PageHome
