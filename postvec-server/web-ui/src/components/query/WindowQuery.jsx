import React, { useState } from 'react'
import {
  useStore,
  requestPath,
  getCurrentModel,
  getCurrentContract,
  getCurrentQueryString,
  getClusterQueryPeersActive,
  getCurrentModelQueryLoading,
  getCurrentModelQueryResponses,
  isEmbedModel,
} from '@/store'
import { Spacing, Button, Text, colors } from '../../shared/react-elemental'
import CustomTabs from './CustomTabs'
import CodeEditor from '../CodeEditor'
import AreaModelsList from './AreaModelsList'

const CONTRACTS = [
  { value: 'native', label: 'Native' },
  { value: 'openai', label: 'OpenAI' },
]

const isJson = (text) => {
  try {
    JSON.parse(text)
    return true
  } catch {
    return false
  }
}

const WindowQuery = () => {
  const model = useStore(getCurrentModel)
  const contract = useStore(getCurrentContract)
  const query = useStore(getCurrentQueryString)
  const peers = useStore(getClusterQueryPeersActive)
  const loading = useStore(getCurrentModelQueryLoading)
  const responses = useStore(getCurrentModelQueryResponses)
  const executeQuery = useStore((state) => state.executeQuery)
  const cacheQuery = useStore((state) => state.cacheQuery)
  const setContract = useStore((state) => state.setContract)
  const [tab, setTab] = useState('query')

  if (!model) {
    return (
      <div className="postvec-placeholder">
        <Text size="kilo" color={colors.gray40}>
          No model is loaded on this node. Put one on disk with `postvec model pull` and load it
          with `postvec-server load`, then refresh.
        </Text>
      </div>
    )
  }

  const valid = isJson(query)
  const activeTab = responses.some((r) => r.peer === tab) ? tab : 'query'
  const tabs = [
    { value: 'query', label: 'Query' },
    ...responses.map((r) => ({
      value: r.peer,
      label: `${r.peer} (${r.ok ? 'ok' : `fail${r.status ? ` ${r.status}` : ''}`})`,
    })),
  ]
  const shown = responses.find((r) => r.peer === activeTab)
  const buttonText = loading
    ? 'Sending..'
    : !valid
      ? 'JSON invalid'
      : peers.length === 0
        ? 'No live nodes'
        : 'Query service'

  return (
    <div className="postvec-layout">
      <div className="postvec-sidebar">
        <AreaModelsList />
      </div>
      <div className="postvec-main">
        <div className="postvec-toolbar">
          <CustomTabs options={tabs} value={activeTab} onChange={setTab} />
          {isEmbedModel(model) && (
            <CustomTabs
              options={CONTRACTS}
              value={contract}
              onChange={(value) => setContract(model.name, value)}
              tabClassName="contract-item"
            />
          )}
        </div>
        <div className="postvec-request-line">
          POST {peers[0] || ''}
          {requestPath(contract, model.name)}
        </div>
        <Spacing bottom size="small">
          {activeTab === 'query' ? (
            <CodeEditor
              height={360}
              value={query}
              onChange={(text) => cacheQuery(model.name, contract, text)}
            />
          ) : (
            <CodeEditor value={JSON.stringify(shown.body, null, 2)} readOnly />
          )}
        </Spacing>
        <div className="postvec-actions">
          <Button
            text={buttonText}
            disabled={loading || !valid || peers.length === 0}
            onClick={() => executeQuery(model, contract, peers, query)}
          />
        </div>
      </div>
    </div>
  )
}

export default WindowQuery
