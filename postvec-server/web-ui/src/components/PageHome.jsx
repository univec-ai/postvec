import React from 'react'
import { useStore } from '@/store'
import { Spacing, Tabs, Text } from '../shared/react-elemental'
import WindowQuery from './query/WindowQuery'
import WindowRegistries from './registries/WindowRegistries'

const SecondaryTabOption = ({ children }) => (
  <Spacing size="small" left right>
    <Spacing size="tiny" top bottom>
      <Text>{children}</Text>
    </Spacing>
  </Spacing>
)

const PageHome = () => {
  const currentTab = useStore((state) => state.prefs.main_menu_tab)
  const setMainMenuTab = useStore((state) => state.setMainMenuTab)
  const validTab = currentTab === 'modelhub' ? 'queries' : currentTab

  let pageContent = null
  switch (validTab) {
    case 'queries':
      pageContent = <WindowQuery />
      break
    case 'registries':
      pageContent = <WindowRegistries />
      break
    default:
      break
  }

  return (
    <section className="postvec-page">
      <div className="container">
        <div className="main-menu">
          <Spacing bottom>
            <Tabs
              options={[
                { value: 'queries', label: <SecondaryTabOption>Queries</SecondaryTabOption> },
                { value: 'registries', label: <SecondaryTabOption>Registries</SecondaryTabOption> },
              ]}
              value={validTab}
              onChange={setMainMenuTab}
              secondary
              fit
            />
          </Spacing>
        </div>
        <div className="p-t-20 p-b-20 p-l-20 p-r-20 postvec-workspace">{pageContent}</div>
      </div>
    </section>
  )
}

export default PageHome
