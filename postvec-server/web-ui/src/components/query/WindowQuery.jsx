import React, { useState } from 'react'
import { useStore } from '@/store'
import { Spacing, Button, Label } from '../../shared/react-elemental'
import CustomTabs from './CustomTabs'
import CodeEditor from '../CodeEditor'
import AreaModelsList from './AreaModelsList'
import {
  getCurrentModel,
  getCurrentModelInputs,
  getCurrentQueryString,
  getClusterQueryPeersActive,
  getCurrentModelQueryLoading,
  getCurrentModelQueryResponses,
} from '@/store'

const SecondaryTabOption = ({ children }) => <div>{children}</div>

const WindowQuery = () => {
  const current_model = useStore(getCurrentModel)
  const current_model_inputs = useStore(getCurrentModelInputs)
  const query_loading = useStore(getCurrentModelQueryLoading)
  const query_responses = useStore(getCurrentModelQueryResponses)
  const current_query_string = useStore(getCurrentQueryString)
  const cluster_query_peers_active = useStore(getClusterQueryPeersActive)
  const executeQuery = useStore((state) => state.executeQuery)
  const cacheQuery = useStore((state) => state.cacheQuery)

  const [tab, setTab] = useState('query')
  const [query_json_valid, setQueryJsonValid] = useState(true)
  const [editor_text, setEditorText] = useState('')

  const onQueryChange = (text) => {
    setEditorText(text)
    cacheQuery(text)
    try {
      JSON.parse(text)
      setQueryJsonValid(true)
    } catch {
      setQueryJsonValid(false)
    }
  }

  const queryService = () => {
    if (current_query_string && current_query_string.length > 0) {
      executeQuery(current_model, cluster_query_peers_active, current_query_string)
    }
  }

  const onAutocorrectClick = () => {
    try {
      JSON.parse(editor_text)
    } catch {
      let text = editor_text.replace(/(?:\\[rn])+/g, '')
      text = text.replace(/(?:\\)+/g, '')
      text = text.replace(/\s\s+/g, ' ')
      const data = current_model_inputs.reduce((accumulator, input) => {
        accumulator[input] = Object.keys(accumulator).length === 0 ? text : ''
        return accumulator
      }, {})
      cacheQuery(JSON.stringify(data, null, 2))
    }
  }

  const hasNodePeers =
    cluster_query_peers_active instanceof Array && cluster_query_peers_active.length > 0
  const queryButtonEnabled = query_json_valid && hasNodePeers && !query_loading
  let submitButtonMessage = 'Query service'
  if (!queryButtonEnabled) {
    if (!query_json_valid) {
      submitButtonMessage = 'JSON invalid'
    } else if (query_loading) {
      submitButtonMessage = 'Sending..'
    } else if (!hasNodePeers) {
      submitButtonMessage = 'No live nodes'
    }
  }

  const jsonStateStr = query_json_valid ? (
    <Label sublabel="Ok" />
  ) : (
    <Spacing bottom>
      <Button text="Auto-correct json" onClick={onAutocorrectClick} />
    </Spacing>
  )

  const actionsArea = (
    <div>
      <div className="pull-left">{jsonStateStr}</div>
      <div className="pull-right">
        <Spacing bottom>
          <Button
            text={submitButtonMessage}
            disabled={!queryButtonEnabled}
            onClick={queryService}
          />
        </Spacing>
      </div>
      <div className="clearfix" />
    </div>
  )

  let activeTab = tab
  if (!activeTab || query_responses.length === 0) {
    activeTab = 'query'
  }
  const tabs = query_responses.map((response) => {
    const label = `${response.peer}${response.success ? ' (ok)' : ' (fail)'}`
    return { value: response.peer, label: <SecondaryTabOption>{label}</SecondaryTabOption> }
  })
  tabs.unshift({ value: 'query', label: <SecondaryTabOption>Query</SecondaryTabOption> })

  let currentText = '{}'
  let activeEditor = null
  if (tab === 'query') {
    currentText = current_query_string
    activeEditor = (
      <CodeEditor
        height={360}
        maxHeight={360}
        value={currentText}
        onChange={onQueryChange}
        language="json"
      />
    )
  } else {
    const texts = query_responses
      .filter((response) => response.peer === activeTab)
      .map((response) => {
        if (response.success) {
          return JSON.stringify(response.result, null, 2)
        }
        return JSON.stringify(
          {
            success: false,
            error: { message: response.error?.message || String(response.error) },
          },
          null,
          2,
        )
      })
    if (texts.length > 0) {
      currentText = texts[0]
    }
    activeEditor = <CodeEditor maxHeight={Infinity} value={currentText} language="json" />
  }

  return (
    <div className="postvec-layout">
      <div className="postvec-sidebar">
        <AreaModelsList showLivePeers={true} />
      </div>
      <div className="postvec-main">
        <CustomTabs
          options={tabs}
          value={activeTab}
          onChange={setTab}
          tabClassName="tab-item"
        />
        <div className="postvec-query-container">
          <Spacing bottom size="small">
            {activeEditor}
          </Spacing>
          <div className="background-paper">{actionsArea}</div>
        </div>
      </div>
    </div>
  )
}

export default WindowQuery
