import axios from 'axios'
import { modelCache } from '../slices/prefsSlice'

export const requestPath = (contract, model) =>
  contract === 'openai' ? '/api/openai/embeddings' : `/api/${encodeURIComponent(model)}`

const failMessage = (status, data, fallback, manage) => {
  if (manage && status === 404) {
    return 'public model management is disabled; restart with --manage or use the loopback admin port'
  }
  const err = data?.error
  if (typeof err === 'string' && err) return err
  if (err?.message) return err.message
  return fallback || `HTTP ${status}`
}

const envelope = async (request, { manage } = {}) => {
  const { status, data } = await request
  if (status >= 400 || data?.success === false) {
    throw new Error(failMessage(status, data, 'request failed', manage))
  }
  return data.data
}

// One request to one peer; never throws. `body` is whatever the node
// returned (native envelope or OpenAI shape), `ok` whether it succeeded.
const post = async (peer, path, payload, timeout) => {
  const started = performance.now()
  const ms = () => Math.round(performance.now() - started)
  try {
    const { status, data } = await axios.post(peer + path, payload, {
      validateStatus: () => true,
      timeout,
    })
    const ok = status < 300 && data?.success !== false
    return { peer, ok, status, ms: ms(), body: data }
  } catch (err) {
    return { peer, ok: false, status: 0, ms: ms(), body: { error: { message: err.message } } }
  }
}

const authHeaders = (get) => {
  const key = get().registry.key
  return key ? { headers: { Authorization: `Bearer ${key}` } } : {}
}

export const createAsyncActions = (set, get) => ({
  appMounted: () => get().getConfiguration(),

  getConfiguration: async () => {
    set((draft) => {
      draft.loading = true
    })
    try {
      const { data } = await axios.get(get().api_endpoint + '/config', { timeout: 15_000 })
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
    const timeout = (get().server?.predict_timeout_ms || 30_000) + 5_000
    const responses = await Promise.all(peers.map((peer) => post(peer, path, payload, timeout)))
    set((draft) => {
      const cache = modelCache(draft, name)
      cache.loading = false
      cache.responses = responses
    })
  },

  loadRegistry: async () => {
    const ep = get().api_endpoint
    const opts = { validateStatus: () => true }
    set((draft) => {
      draft.registry.loading = true
      draft.registry.error = null
    })
    try {
      const [installed, pulls, available] = await Promise.all([
        envelope(axios.get(ep + '/api/registry/models', opts)),
        envelope(axios.get(ep + '/api/registry/pulls', opts)),
        envelope(
          axios.get(ep + '/api/registry/available', { ...opts, ...authHeaders(get) }),
        ).catch((err) => ({
          _error: err.message,
        })),
      ])
      set((draft) => {
        draft.registry.installed = installed.models || []
        draft.registry.pulls = pulls.pulls || []
        if (available._error) {
          draft.registry.error = available._error
          draft.registry.available = []
          draft.registry.channel = null
          draft.registry.authenticated = false
          draft.registry.signed_in_as = null
        } else {
          draft.registry.available = available.models || []
          draft.registry.channel = available.channel || null
          draft.registry.authenticated = !!available.authenticated
          draft.registry.signed_in_as = available.signed_in_as || null
        }
      })
    } catch (err) {
      set((draft) => {
        draft.registry.error = err.message
      })
    } finally {
      set((draft) => {
        draft.registry.loading = false
      })
    }
  },

  loadPulls: async () => {
    try {
      const data = await envelope(
        axios.get(get().api_endpoint + '/api/registry/pulls', { validateStatus: () => true }),
      )
      set((draft) => {
        draft.registry.pulls = data.pulls || []
      })
    } catch {
      // A missed poll is retried; the next loadRegistry surfaces a real error.
    }
  },

  // Installed + pulls only: what activate, deactivate and remove change.
  // The catalogue is a network round trip and does not move.
  loadInstalled: async () => {
    try {
      const data = await envelope(
        axios.get(get().api_endpoint + '/api/registry/models', { validateStatus: () => true }),
      )
      set((draft) => {
        draft.registry.installed = data.models || []
      })
    } catch (err) {
      set((draft) => {
        draft.registry.error = err.message
      })
    }
    await get().loadPulls()
  },

  // One mutation: POST, fail on the first per-model error, refresh what changed.
  manageModels: async (kind, models, extra = {}) => {
    set((draft) => {
      draft.registry.busy = { kind, name: models[0] }
      draft.registry.error = null
    })
    try {
      const data = await envelope(
        axios.post(
          `${get().api_endpoint}/api/registry/${kind}`,
          { models, ...extra },
          { validateStatus: () => true, ...authHeaders(get) },
        ),
        { manage: true },
      )
      const failed = (data.results || []).find((r) => r.status === 'error')
      await Promise.all([
        kind === 'remove' ? get().loadRegistry() : get().loadInstalled(),
        kind !== 'pull' && get().getConfiguration(),
      ])
      if (failed) throw new Error(`${failed.model}: ${failed.error}`)
    } catch (err) {
      set((draft) => {
        draft.registry.error = err.message
      })
    } finally {
      set((draft) => {
        draft.registry.busy = null
      })
    }
  },
})
