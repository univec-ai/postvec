import React, { useState } from 'react'
import {
  useStore,
  requestPath,
  getCurrentModel,
  getCurrentContract,
  getCurrentQueryString,
  getClusterMembers,
  getClusterQueryPeersActive,
  getCurrentModelQueryLoading,
  getCurrentModelQueryResponses,
  isEmbedModel,
} from '@/store'
import CodeEditor from './CodeEditor'

const isJson = (text) => {
  try {
    JSON.parse(text)
    return true
  } catch {
    return false
  }
}

const Meta = ({ model }) => {
  const params = model.configuration?.params || {}
  const chips = isEmbedModel(model)
    ? [
        model.configuration?.backend || model.provider,
        'embed',
        params.target_dim && `${params.target_dim} dims`,
        params.sequence_len && `${params.sequence_len} tokens`,
      ]
    : [
        model.configuration?.backend || model.provider,
        'convert',
        params.source_model && `${params.source_model} → ${params.target_model}`,
        params.source_dim && `${params.source_dim} → ${params.target_dim} dims`,
      ]
  return (
    <div className="topbar-meta">
      {chips.filter(Boolean).map((c) => (
        <span className="chip" key={c}>
          {c}
        </span>
      ))}
    </div>
  )
}

const Response = ({ responses, loading }) => {
  const [tab, setTab] = useState(null)
  if (loading) return <div className="placeholder">Waiting for the node…</div>
  if (responses.length === 0) {
    return (
      <div className="placeholder">
        <p>
          Run the request to see the node&apos;s raw reply here. <kbd>Ctrl</kbd> + <kbd>Enter</kbd>{' '}
          in the editor works too.
        </p>
      </div>
    )
  }
  const shown = responses.find((r) => r.peer === tab) || responses[0]
  return (
    <>
      <div className="pane-head">
        {responses.length > 1 ? (
          <div className="resp-tabs">
            {responses.map((r) => (
              <button
                key={r.peer}
                className={`resp-tab${r === shown ? ' active' : ''}`}
                onClick={() => setTab(r.peer)}
              >
                {r.peer.replace(/^https?:\/\//, '')}
              </button>
            ))}
          </div>
        ) : (
          <span className="muted">{shown.peer}</span>
        )}
        <span className={`status ${shown.ok ? 'ok' : 'err'}`}>
          {shown.ok ? 'OK' : 'Failed'}
          {shown.status ? ` · ${shown.status}` : ''} · {shown.ms} ms
        </span>
      </div>
      <div className="pane-body">
        <CodeEditor value={JSON.stringify(shown.body, null, 2)} readOnly />
      </div>
    </>
  )
}

const Workbench = () => {
  const model = useStore(getCurrentModel)
  const contract = useStore(getCurrentContract)
  const query = useStore(getCurrentQueryString)
  const members = useStore(getClusterMembers)
  const peers = useStore(getClusterQueryPeersActive)
  const loading = useStore(getCurrentModelQueryLoading)
  const responses = useStore(getCurrentModelQueryResponses)
  const { executeQuery, cacheQuery, resetQuery, setContract, toggleQueryPeer } =
    useStore.getState()

  if (!model) {
    return (
      <div className="page">
        <h1>No model to query</h1>
        <p>
          This node has nothing loaded. Put a model on disk with{' '}
          <code>postvec model pull</code>, load it with <code>postvec-server load</code>, and it
          appears here on the next refresh.
        </p>
      </div>
    )
  }

  const valid = isJson(query)
  const run = () => valid && !loading && executeQuery(model, contract, peers, query)

  return (
    <>
      <div className="topbar">
        <div>
          <h1>{model.name}</h1>
          <Meta model={model} />
        </div>
        {isEmbedModel(model) && (
          <div className="segmented" role="tablist">
            {[
              ['native', 'Native'],
              ['openai', 'OpenAI'],
            ].map(([value, label]) => (
              <button
                key={value}
                className={contract === value ? 'active' : ''}
                onClick={() => setContract(model.name, value)}
              >
                {label}
              </button>
            ))}
          </div>
        )}
      </div>
      <div className="workbench">
        <section className="pane">
          <div className="pane-head">
            <span className="route">
              <b>POST</b> {peers[0]}
              {requestPath(contract, model.name)}
            </span>
            <button className="link" onClick={() => resetQuery(model.name, contract)}>
              Reset
            </button>
          </div>
          <div className="pane-body">
            <CodeEditor
              value={query}
              onChange={(text) => cacheQuery(model.name, contract, text)}
              onSubmit={run}
            />
          </div>
          {members.length > 1 && (
            <div className="pane-foot">
              <span>Send to</span>
              <div className="targets">
                {members.map((peer) => (
                  <button
                    key={peer}
                    className={`target${peers.includes(peer) ? ' on' : ''}`}
                    onClick={() => toggleQueryPeer(peer, !peers.includes(peer))}
                  >
                    {peer.replace(/^https?:\/\//, '')}
                  </button>
                ))}
              </div>
            </div>
          )}
          <div className="pane-foot">
            <span>{valid ? 'JSON body' : 'Body is not valid JSON'}</span>
            <button className="btn primary" disabled={!valid || loading} onClick={run}>
              {loading ? 'Sending…' : 'Send request'}
            </button>
          </div>
        </section>
        <section className="pane">
          <Response responses={responses} loading={loading} />
        </section>
      </div>
    </>
  )
}

export default Workbench
