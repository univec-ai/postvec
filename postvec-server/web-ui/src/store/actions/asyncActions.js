import axios from 'axios'
import { modelCache } from '../slices/prefsSlice'

export const requestPath = (contract, model) =>
  contract === 'openai' ? '/api/openai/embeddings' : `/api/${encodeURIComponent(model)}`

// One request to one peer; never throws. `body` is whatever the node
// returned (native envelope or OpenAI shape), `ok` whether it succeeded.
const post = async (peer, path, payload) => {
  const started = performance.now()
  const ms = () => Math.round(performance.now() - started)
  try {
    const { status, data } = await axios.post(peer + path, payload, {
      validateStatus: () => true,
    })
    const ok = status < 300 && data?.success !== false
    return { peer, ok, status, ms: ms(), body: data }
  } catch (err) {
    return { peer, ok: false, status: 0, ms: ms(), body: { error: { message: err.message } } }
  }
}

export const createAsyncActions = (set, get) => ({
  appMounted: () => get().getConfiguration(),

  getConfiguration: async () => {
    set((draft) => {
      draft.loading = true
    })
    try {
      const { data } = await axios.get(get().api_endpoint + '/config')
      if (!data?.success) throw new Error(data?.error?.message || 'unexpected /config reply')
      set((draft) => {
        draft.models = data.data.models || []
        draft.cluster = data.data.cluster?.nodes || []
        draft.system = data.data.system || null
        draft.server = data.data.server || null
        draft.error = null
      })
    } catch (err) {
      set((draft) => {
        draft.error = `Cannot reach ${get().api_endpoint}/config: ${err.message}`
      })
    } finally {
      set((draft) => {
        draft.loading = false
      })
    }
  },

  executeQuery: async (model, contract, peers, text) => {
    let payload
    try {
      payload = JSON.parse(text)
    } catch {
      return
    }
    const name = model.name
    set((draft) => {
      const cache = modelCache(draft, name)
      cache.loading = true
      cache.responses = []
    })
    const path = requestPath(contract, name)
    const responses = await Promise.all(peers.map((peer) => post(peer, path, payload)))
    set((draft) => {
      const cache = modelCache(draft, name)
      cache.loading = false
      cache.responses = responses
    })
  },
})
